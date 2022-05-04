/*
 * Copyright 2018-2021 TON DEV SOLUTIONS LTD.
 *
 * Licensed under the SOFTWARE EVALUATION License (the "License"); you may not use
 * this file except in compliance with the License.
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific TON DEV software governing permissions and
 * limitations under the License.
 */
use crate::config::Config;
use crate::{convert, DebugLogger};
use crate::helpers::{
    TonClient, now, now_ms, create_client_verbose, load_abi,
    query_account_field, TRACE_PATH, SDK_EXECUTION_ERROR_CODE, create_client
};
use ton_abi::{Contract, ParamType};
use chrono::{TimeZone, Local};

use ton_client::abi::{
    encode_message,
    decode_message,
    ParamsOfDecodeMessage,
    ParamsOfEncodeMessage,
    Abi,
    FunctionHeader,
};
use ton_client::processing::{
    ParamsOfSendMessage,
    ParamsOfWaitForTransaction,
    ParamsOfProcessMessage,
    ProcessingEvent,
    wait_for_transaction,
    send_message,
};
use ton_client::tvm::{
    run_executor,
    ParamsOfRunExecutor,
    AccountForExecutor
};
use ton_block::{Account, Serializable, Deserializable, Message, MsgAddressInt, ExternalInboundMessageHeader, Grams};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};
use serde_json::{Value};
use ton_block::MsgAddressExt::AddrNone;
use ton_client::error::ClientError;
use ton_types::{BuilderData, Cell, IBitstring, SliceData};
use crate::debug::execute_debug;
use crate::debug_executor::TraceLevel;
use crate::message::{EncodedMessage, prepare_message_params, print_encoded_message, unpack_message};
use crate::replay::{CONFIG_ADDR, construct_blockchain_config};

const PREFIX_UPDATE_CONFIG_MESSAGE_DATA: &str = "43665021";

pub fn serialize_config_param(config_str: String) -> Result<(Cell, u32), String> {
    let config_json: serde_json::Value = serde_json::from_str(&*config_str)
        .map_err(|e| format!(r#"failed to parse "new_param_file": {}"#, e))?;
    let config_json = config_json.as_object()
        .ok_or(format!(r#""new_param_file" is not json object"#))?;
    if config_json.len() != 1 {
        Err(r#""new_param_file" is not a valid json"#.to_string())?;
    }

    let mut key_number = None;
    for key in config_json.keys() {
        if !key.starts_with("p") {
            Err(r#""new_param_file" is not a valid json"#.to_string())?;
        }
        key_number = Some(key.trim_start_matches("p").to_string());
        break;
    }

    let key_number = key_number
        .ok_or(format!(r#""new_param_file" is not a valid json"#))?
        .parse::<u32>()
        .map_err(|e| format!(r#""new_param_file" is not a valid json: {}"#, e))?;

    let config_params = ton_block_json::parse_config(config_json)
        .map_err(|e| format!(r#"failed to parse config params from "new_param_file": {}"#, e))?;

    let config_param = config_params.config(key_number)
        .map_err(|e| format!(r#"failed to parse config params from "new_param_file": {}"#, e))?
        .ok_or(format!(r#"Not found config number {} in parsed config_params"#, key_number))?;

    let mut cell = BuilderData::default();
    config_param.write_to_cell(&mut cell)
        .map_err(|e| format!(r#"failed to serialize config param": {}"#, e))?;
    let config_cell = cell.references()[0].clone();

    Ok((config_cell, key_number))
}

pub fn prepare_message_new_config_param(
    config_param: Cell,
    seqno: u32,
    key_number: u32,
    config_account: SliceData,
    private_key_of_config_account: Vec<u8>
) -> Result<Message, String> {
    let prefix = hex::decode(PREFIX_UPDATE_CONFIG_MESSAGE_DATA).unwrap();
    let since_the_epoch = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as u32 + 100; // timestamp + 100 secs

    let mut cell = BuilderData::default();
    cell.append_raw(prefix.as_slice(), 32).unwrap();
    cell.append_u32(seqno).unwrap();
    cell.append_u32(since_the_epoch).unwrap();
    cell.append_i32(key_number as i32).unwrap();
    cell.append_reference_cell(config_param.clone());

    let exp_key = ed25519_dalek::ExpandedSecretKey::from(
        &ed25519_dalek::SecretKey::from_bytes(private_key_of_config_account.as_slice()
    )
        .map_err(|e| format!(r#"failed to read private key from config-master file": {}"#, e))?);
    let pub_key = ed25519_dalek::PublicKey::from(&exp_key);
    let msg_signature = exp_key.sign(cell.finalize(0).unwrap().repr_hash().into_vec().as_slice(), &pub_key).to_bytes().to_vec();

    let mut cell = BuilderData::default();
    cell.append_raw(msg_signature.as_slice(), 64*8).unwrap();
    cell.append_raw(prefix.as_slice(), 32).unwrap();
    cell.append_u32(seqno).unwrap();
    cell.append_u32(since_the_epoch).unwrap();
    cell.append_i32(key_number as i32).unwrap();
    cell.append_reference_cell(config_param);

    let config_contract_address = MsgAddressInt::with_standart(None, -1, config_account).unwrap();
    let mut header = ExternalInboundMessageHeader::new(AddrNone, config_contract_address);
    header.import_fee = Grams::zero();
    let message = Message::with_ext_in_header_and_body(header, cell.into());

    Ok(message)
}

async fn decode_call_parameters(ton: TonClient, msg: &EncodedMessage, abi: Abi) -> Result<(String, String), String> {
    let result = decode_message(
        ton,
        ParamsOfDecodeMessage {
            abi,
            message: msg.message.clone(),
            ..Default::default()
        },
    )
    .await
    .map_err(|e| format!("couldn't decode message: {}", e))?;

    Ok((
        result.name,
        serde_json::to_string_pretty(
            &result.value.unwrap_or(json!({}))
        ).map_err(|e| format!("failed to serialize result: {}", e))?
    ))
}

fn parse_integer_param(value: &str) -> Result<String, String> {
    let value = value.trim_matches('\"');

    if value.ends_with('T') {
        convert::convert_token(value.trim_end_matches('T'))
    } else {
        Ok(value.to_owned())
    }
}

fn build_json_from_params(params_vec: Vec<&str>, abi: &str, method: &str) -> Result<String, String> {
    let abi_obj = Contract::load(abi.as_bytes()).map_err(|e| format!("failed to parse ABI: {}", e))?;
    let functions = abi_obj.functions();

    let func_obj = functions.get(method).ok_or("failed to load function from abi")?;
    let inputs = func_obj.input_params();

    let mut params_json = json!({ });
    for input in inputs {
        let mut iter = params_vec.iter();
        let _param = iter.find(|x| x.trim_start_matches('-') == input.name)
            .ok_or(format!(r#"argument "{}" of type "{}" not found"#, input.name, input.kind))?;

        let value = iter.next()
            .ok_or(format!(r#"argument "{}" of type "{}" has no value"#, input.name, input.kind))?
            .to_string();

        let value = match input.kind {
            ParamType::Uint(_) | ParamType::Int(_) => {
                json!(parse_integer_param(&value)?)
            },
            ParamType::Array(ref x) => {
                if let ParamType::Uint(_) = **x {
                    let mut result_vec: Vec<String> = vec![];
                    for i in value.split(|c| c == ',' || c == '[' || c == ']') {
                        if !i.is_empty() {
                            result_vec.push(parse_integer_param(i)?)
                        }
                    }
                    json!(result_vec)
                } else {
                    json!(value)
                }
            },
            _ => {
                json!(value)
            }
        };
        params_json[input.name.clone()] = value;
    }

    serde_json::to_string(&params_json).map_err(|e| format!("{}", e))
}

pub async fn emulate_locally(
    ton: TonClient,
    addr: &str,
    msg: String,
    is_fee: bool,
) -> Result<(), String> {
    let state: String;
    let state_boc = query_account_field(ton.clone(), addr, "boc").await;
    if state_boc.is_err() {
        if is_fee {
            let addr = ton_block::MsgAddressInt::from_str(addr)
                .map_err(|e| format!("couldn't decode address: {}", e))?;
            state = base64::encode(
                &ton_types::cells_serialization::serialize_toc(
                    &Account::with_address(addr)
                        .serialize()
                        .map_err(|e| format!("couldn't create dummy account for deploy emulation: {}", e))?
                ).map_err(|e| format!("failed to serialize account cell: {}", e))?
            );
        } else {
            return Err(state_boc.err().unwrap());
        }
    } else {
        state = state_boc.unwrap();
    }
    let res = run_executor(
        ton.clone(),
        ParamsOfRunExecutor {
            message: msg.clone(),
            account: AccountForExecutor::Account {
                boc: state,
                unlimited_balance: if is_fee {
                    Some(true)
                } else {
                    None
                },
            },
            ..Default::default()
        },
    )
    .await;

    if res.is_err() {
        return Err(format!("{:#}", res.err().unwrap()));
    }
    if is_fee {
        let fees = res.unwrap().fees;
        println!("{{");
        println!("  \"in_msg_fwd_fee\": \"{}\",", fees.in_msg_fwd_fee);
        println!("  \"storage_fee\": \"{}\",", fees.storage_fee);
        println!("  \"gas_fee\": \"{}\",", fees.gas_fee);
        println!("  \"out_msgs_fwd_fee\": \"{}\",", fees.out_msgs_fwd_fee);
        println!("  \"total_account_fees\": \"{}\",", fees.total_account_fees);
        println!("  \"total_output\": \"{}\"", fees.total_output);
        println!("}}");
    } else {
        println!("Local run succeeded. Executing onchain."); // TODO: check is_json
    }
    Ok(())
}

pub async fn send_message_and_wait(
    ton: TonClient,
    abi: Option<Abi>,
    msg: String,
    config: &Config,
) -> Result<serde_json::Value, String> {

    if !config.is_json {
        println!("Processing... ");
    }
    let callback = |_| {
        async move {}
    };
    let result = send_message(
        ton.clone(),
        ParamsOfSendMessage {
            message: msg.clone(),
            abi: abi.clone(),
            send_events: false,
            ..Default::default()
        },
        callback,
    ).await
        .map_err(|e| format!("{:#}", e))?;

    if !config.async_call {
        let result = wait_for_transaction(
            ton.clone(),
            ParamsOfWaitForTransaction {
                abi,
                message: msg.clone(),
                shard_block_id: result.shard_block_id,
                send_events: true,
                ..Default::default()
            },
            callback,
        ).await
            .map_err(|e| format!("{:#}", e))?;
        Ok(result.decoded.and_then(|d| d.output).unwrap_or(json!({})))
    } else {
        Ok(json!({}))
    }
}

pub async fn process_message(
    ton: TonClient,
    msg: ParamsOfEncodeMessage,
    config: &Config,
) -> Result<serde_json::Value, ClientError> {
    let callback = |event| { async move {
        if let ProcessingEvent::DidSend { shard_block_id: _, message_id, message: _ } = event {
            println!("MessageId: {}", message_id)
        }
    }};
    let res = if !config.is_json {
        ton_client::processing::process_message(
            ton.clone(),
            ParamsOfProcessMessage {
                message_encode_params: msg.clone(),
                send_events: true,
                ..Default::default()
            },
            callback,
        ).await
    } else {
        ton_client::processing::process_message(
            ton.clone(),
            ParamsOfProcessMessage {
                message_encode_params: msg.clone(),
                send_events: true,
                ..Default::default()
            },
            |_| { async move {} },
        ).await
    }?;

    Ok(res.decoded.and_then(|d| d.output).unwrap_or(json!({})))
}

pub async fn call_contract_with_result(
    config: &Config,
    addr: &str,
    abi: String,
    method: &str,
    params: &str,
    keys: Option<String>,
    is_fee: bool,
    dbg_info: Option<String>,
) -> Result<serde_json::Value, String> {
    let ton = if config.debug_fail.enabled() {
        log::set_max_level(log::LevelFilter::Trace);
        log::set_boxed_logger(
            Box::new(DebugLogger::new(TRACE_PATH.to_string()))
        ).map_err(|e| format!("Failed to set logger: {}", e))?;
        create_client(config)?
    } else {
        create_client_verbose(config)?
    };
    call_contract_with_client(ton, config, addr, abi, method, params, keys, is_fee, dbg_info).await
}

pub async fn call_contract_with_client(
    ton: TonClient,
    config: &Config,
    addr: &str,
    abi_string: String,
    method: &str,
    params: &str,
    keys: Option<String>,
    is_fee: bool,
    dbg_info: Option<String>,
) -> Result<serde_json::Value, String> {
    let abi = load_abi(&abi_string)?;

    let expire_at = config.lifetime + now()?;
    let time = now_ms();
    let header = FunctionHeader {
        expire: Some(expire_at),
        time: Some(time),
        ..Default::default()
    };
    let msg_params = prepare_message_params(
        addr,
        abi.clone(),
        method,
        params,
        Some(header),
        keys.clone(),
    )?;

    let needs_encoded_msg = is_fee ||
        config.async_call ||
        config.local_run ||
        config.debug_fail.enabled();

    let message = if needs_encoded_msg {
        let msg = encode_message(ton.clone(), msg_params.clone()).await
            .map_err(|e| format!("failed to create inbound message: {}", e))?;

        if config.local_run || is_fee {
            emulate_locally(ton.clone(), addr, msg.message.clone(), is_fee).await?;
            if is_fee {
                return Ok(Value::Null);
            }
        }
        if config.async_call {
            return send_message_and_wait(ton,
                                         Some(abi),
                                         msg.message.clone(),
                                         config).await;
        }
        Some(msg.message)
    } else {
        None
    };

    if !config.is_json {
        print!("Expire at: ");
        let expire_at = Local.timestamp(expire_at as i64 , 0);
        println!("{}", expire_at.to_rfc2822());
    }

    let dump = if config.debug_fail.enabled() {
        let acc_boc = query_account_field(
            ton.clone(),
            addr,
            "boc",
        ).await?;
        let account = Account::construct_from_base64(&acc_boc)
            .map_err(|e| format!("Failed to construct account: {}", e))?
            .serialize()
            .map_err(|e| format!("Failed to serialize account: {}", e))?;

        let config_acc = query_account_field(
            ton.clone(),
            CONFIG_ADDR,
            "boc",
        ).await?;

        let config_acc = Account::construct_from_base64(&config_acc)
            .map_err(|e| format!("Failed to construct config account: {}", e))?;
        let bc_config = construct_blockchain_config(&config_acc)?;
        let now = now_ms();
        Some((bc_config, account, message.unwrap(), now))
    } else {
        None
    };

    let res = process_message(ton.clone(), msg_params, config).await;

    if config.debug_fail.enabled() && res.is_err()
        && res.clone().err().unwrap().code == SDK_EXECUTION_ERROR_CODE {
        if config.is_json {
            println!("{:#}", res.clone().err().unwrap());
        } else {
            println!("Error: {:#}", res.clone().err().unwrap());
            println!("Execution failed. Starting debug...");
        }
        let (bc_config, mut account, message, now) = dump.unwrap();
        let message = Message::construct_from_base64(&message)
            .map_err(|e| format!("failed to construct message: {}", e))?;
        let _ = execute_debug(Some(bc_config), None, &mut account, Some(&message), (now / 1000) as u32, now,now, dbg_info,config.debug_fail == TraceLevel::Full, false).await?;

        if !config.is_json {
            println!("Debug finished.");
            println!("Log saved to {}", TRACE_PATH);
        }
        return Err("".to_string());
    }
    res.map_err(|e| format!("{:#}", e))
}

pub fn print_json_result(result: Value, config: &Config) -> Result<(), String> {
    if !result.is_null() {
        let result = serde_json::to_string_pretty(&result)
            .map_err(|e| format!("Failed to serialize the result: {}", e))?;
        if !config.is_json {
            println!("Result: {}", result);
        } else {
            println!("{}", result);
        }
    }
    Ok(())
}

pub async fn call_contract(
    config: &Config,
    addr: &str,
    abi: String,
    method: &str,
    params: &str,
    keys: Option<String>,
    is_fee: bool,
    dbg_info: Option<String>,
) -> Result<(), String> {
    let result = call_contract_with_result(config, addr, abi, method, params, keys, is_fee, dbg_info).await?;
    if !config.is_json {
        println!("Succeeded.");
    }
    print_json_result(result, config)?;
    Ok(())
}


pub async fn call_contract_with_msg(config: &Config, str_msg: String, abi: String) -> Result<(), String> {
    let ton = create_client_verbose(&config)?;
    let abi = load_abi(&abi)?;

    let (msg, _) = unpack_message(&str_msg)?;
    if config.is_json {
        println!("{{");
    }
    print_encoded_message(&msg, config.is_json);

    let params = decode_call_parameters(ton.clone(), &msg, abi.clone()).await?;

    if !config.is_json {
        println!("Calling method {} with parameters:", params.0);
        println!("{}", params.1);
        println!("Processing... ");
    } else {
        println!("  \"Method\": \"{}\",", params.0);
        println!("  \"Parameters\": {},", params.1);
        println!("}}");
    }
    let result = send_message_and_wait(ton, Some(abi), msg.message,  config).await?;

    if !config.is_json {
        println!("Succeeded.");
        if !result.is_null() {
            println!("Result: {}", serde_json::to_string_pretty(&result)
                .map_err(|e| format!("failed to serialize result: {}", e))?);
        }
    }
    Ok(())
}

pub fn parse_params(params_vec: Vec<&str>, abi: &str, method: &str) -> Result<String, String> {
    if params_vec.len() == 1 {
        // if there is only 1 parameter it must be a json string with arguments
        Ok(params_vec[0].to_owned())
    } else {
        build_json_from_params(params_vec, abi, method)
    }
}
