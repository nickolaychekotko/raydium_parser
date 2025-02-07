use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use futures_util::{StreamExt, SinkExt};
use serde_json::Value;
use solana_sdk::instruction::CompiledInstruction;
use solana_sdk::transaction::VersionedTransaction;
use solana_program::message::VersionedMessage;
use solana_program::instruction::AccountMeta;
use solana_program::message::MessageHeader;
use solana_sdk::pubkey::Pubkey;
use std::fs::OpenOptions;
use std::io::Write;
use base64;
use bincode;
use reqwest::Client;
use carbon_raydium_amm_v4_decoder::{RaydiumAmmV4Decoder, instructions::RaydiumAmmV4Instruction};
use carbon_core::instruction::InstructionDecoder;
use solana_sdk::instruction::Instruction;
use std::str::FromStr;

// RPC-эндпоинты
const RPC_HTTP_URL: &str = "";
const QUICKNODE_WS_URL: &str = "";
const RAYDIUM_PROGRAM_ID: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";

#[tokio::main]
async fn main() {
    connect_to_quicknode_ws().await.expect("Ошибка подключения к WebSocket");
}

// Подключение к WebSocket Solana и подписка на логи Raydium AMM v4
async fn connect_to_quicknode_ws() -> Result<(), Box<dyn std::error::Error>> {
    let (ws_stream, _) = connect_async(QUICKNODE_WS_URL).await.expect("Ошибка подключения к WebSocket");
    let (mut write, mut read) = ws_stream.split();

    let subscription = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "logsSubscribe",
        "params": [
            { "mentions": [RAYDIUM_PROGRAM_ID] },
            { "commitment": "confirmed" }
        ]
    });

    write.send(Message::Text(subscription.to_string())).await.expect("Ошибка отправки подписки");
    println!("Подписаны на WebSocket QuickNode (Raydium AMM v4)");

    let mut initial_slot: Option<u64> = None;

    while let Some(msg) = read.next().await {
        match msg {
            Ok(Message::Text(raw)) => {
                let json_resp: Value = serde_json::from_str(&raw).unwrap_or_else(|_| {
                    println!("Ошибка парсинга JSON: {}", raw);
                    serde_json::json!({})
                });

                let slot = match json_resp["params"]["result"]["context"]["slot"].as_u64() {
                    Some(s) => s,
                    None => {
                        println!("Ошибка: слот отсутствует в ответе!");
                        continue;
                    }
                };

                let signature = json_resp["params"]["result"]["value"]["signature"].as_str().unwrap_or("").to_string();
                println!("Новый слот: {}", slot);

                if initial_slot.is_none() {
                    initial_slot = Some(slot);
                    println!("Стартовый слот: {}", slot);
                }

                if let Some(start_slot) = initial_slot {
                    let slot_diff = slot as i64 - start_slot as i64;
                    println!("Слот {} (разница: {} слотов)", slot, slot_diff);

                    if slot_diff >= 100 {
                        println!("Достигнут предел 100 слотов. Останавливаем подписку.");
                        break;
                    }
                }

                println!("Обнаружена транзакция: {}", signature);
                if let Some(tx) = fetch_transaction(&signature).await {
                    decode_transaction(&signature, &tx, slot).await;
                }
            }
            Err(e) => {
                println!("Ошибка WebSocket: {:?}", e);
                break;
            }
            _ => {}
        }
    }

    Ok(())
}

// Запрашивает полную транзакцию
async fn fetch_transaction(signature: &str) -> Option<VersionedTransaction> {
    let client = Client::new();
    let request_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getTransaction",
        "params": [
            signature,
            { "encoding": "base64", "commitment": "confirmed", "maxSupportedTransactionVersion": 0 }
        ]
    });

    let response = client.post(RPC_HTTP_URL)
        .json(&request_body)
        .send()
        .await.ok()?;

    let json_resp: Value = response.json().await.ok()?;
    if json_resp["result"].is_null() {
        println!("RPC не нашёл транзакцию: {}", signature);
        return None;
    }

    let base64_str = json_resp["result"]["transaction"][0].as_str()?;
    let tx_bytes = base64::decode(base64_str).ok()?;
    let versioned_tx: VersionedTransaction = bincode::deserialize(&tx_bytes).ok()?;
    
    Some(versioned_tx)
}

// Декодирование транзакции и поиск SwapBaseIn
async fn decode_transaction(signature: &str, versioned_tx: &VersionedTransaction, slot: u64) {
    let decoder = RaydiumAmmV4Decoder;

    for cix in versioned_tx.message.instructions() {
        if let Some(ix) = convert_compiled_instruction(cix, &versioned_tx.message) {
            if let Some(decoded_inst) = decoder.decode_instruction(&ix) {
                if let RaydiumAmmV4Instruction::SwapBaseIn(swap_data) = decoded_inst.data {
                    println!("[SwapBaseIn] Signature: {}, amount_in: {}, min_out: {}, slot: {}", signature, swap_data.amount_in, swap_data.minimum_amount_out, slot);
                    save_event(signature, swap_data.amount_in, swap_data.minimum_amount_out, slot);
                }
            }
        }
    }
}

// Преобразует `CompiledInstruction` в `Instruction`
fn convert_compiled_instruction(
    cix: &CompiledInstruction,
    msg: &VersionedMessage,
) -> Option<Instruction> {
    let account_keys: Vec<Pubkey> = msg.static_account_keys().to_vec();
    let program_id_index = cix.program_id_index as usize;

    if program_id_index >= account_keys.len() {
        eprintln!("Ошибка: program_id_index {} выходит за границы account_keys.len() = {}", program_id_index, account_keys.len());
        return None;
    }

    let program_id = account_keys[program_id_index];
    println!(
        "Проверка: program_id_index = {}, account_keys.len() = {}, instruction_accounts.len() = {}",
        program_id_index, account_keys.len(), cix.accounts.len()
    );

    let raydium_program_id = Pubkey::from_str(RAYDIUM_PROGRAM_ID).unwrap();
    if program_id != raydium_program_id {
        return None;
    }

    let header: &MessageHeader = msg.header();

    let num_signers = header.num_required_signatures as usize;
    let num_writable_signers = num_signers - header.num_readonly_signed_accounts as usize;
    let num_writable_accounts = num_writable_signers + header.num_readonly_unsigned_accounts as usize;

    let accounts: Vec<AccountMeta> = cix.accounts.iter().filter_map(|&i| {
        let i = i as usize;
        if i >= account_keys.len() {
            eprintln!("Ошибка: account index {} выходит за границы account_keys", i);
            return None;
        }

        Some(AccountMeta {
            pubkey: account_keys[i],
            is_signer: i < num_signers,
            is_writable: i < num_writable_accounts,
        })
    }).collect();

    println!(
        "Instruction -> program_id: {}, accounts: {}",
        program_id, accounts.len()
    );

    Some(Instruction {
        program_id,
        accounts,
        data: cix.data.clone(),
    })
}


// Сохранение `SwapBaseIn` в JSON
fn save_event(signature: &str, amount_in: u64, min_out: u64, slot: u64) {
    let event = serde_json::json!({
        "transaction_signature": signature,
        "slot": slot,
        "amount_in": amount_in,
        "min_amount_out": min_out
    });

    let mut file = OpenOptions::new().create(true).append(true).open("swap_events.json").expect("Ошибка открытия файла");
    writeln!(file, "{}", event.to_string()).expect("Ошибка записи в файл");
    println!("Событие сохранено в swap_events.json");
}
