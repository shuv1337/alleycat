//! V11 — OpenCode catalog and loaded-thread compatibility.
//!
//! Pins two client-visible shapes that the phone relies on:
//!
//! - modern OpenCode serves `models` as an object map, not an array;
//! - `thread/loaded/list` uses the codex `{ data, nextCursor }` list shape.

#[path = "support/mod.rs"]
mod support;

use serde_json::json;
use support::{FakeServerState, bring_up_bridge, read_until_response, send};

#[tokio::test]
async fn model_list_reads_object_catalog_from_config_providers() {
    let state = std::sync::Arc::new(std::sync::Mutex::new(FakeServerState::default()));
    state.lock().unwrap().route(
        "GET /config/providers",
        json!({
            "providers": [{
                "id": "lmstudio",
                "name": "LM Studio",
                "models": {
                    "qwen/qwen3.5-35b-a3b": {
                        "id": "qwen/qwen3.5-35b-a3b",
                        "name": "Qwen 3.5 35B",
                        "description": "Local model",
                        "capabilities": {
                            "reasoning": true,
                            "input": {
                                "text": true,
                                "image": true
                            }
                        }
                    }
                }
            }],
            "default": {
                "lmstudio": "qwen/qwen3.5-35b-a3b"
            }
        }),
    );
    let mut fx = bring_up_bridge("v11-model-config", state.clone()).await;

    send(&mut fx.write, 2, "model/list", json!({})).await;
    let resp = read_until_response(&mut fx.read, 2).await;
    let data = resp["result"]["data"].as_array().expect("data");
    assert_eq!(data.len(), 1, "{data:#?}");
    assert_eq!(data[0]["id"], "lmstudio/qwen/qwen3.5-35b-a3b");
    assert_eq!(data[0]["model"], "lmstudio/qwen/qwen3.5-35b-a3b");
    assert_eq!(
        data[0]["displayName"],
        "OpenCode / LM Studio / Qwen 3.5 35B"
    );
    assert_eq!(data[0]["description"], "Local model");
    assert_eq!(data[0]["inputModalities"], json!(["text", "image"]));
    assert_eq!(data[0]["isDefault"], true);
    assert!(
        data[0]["supportedReasoningEfforts"]
            .as_array()
            .expect("reasoning efforts")
            .iter()
            .any(|effort| effort["reasoningEffort"] == "high"),
        "{:#?}",
        data[0]["supportedReasoningEfforts"]
    );

    fx.shutdown().await;
}

#[tokio::test]
async fn model_list_falls_back_to_provider_object_catalog() {
    let state = std::sync::Arc::new(std::sync::Mutex::new(FakeServerState::default()));
    state.lock().unwrap().route(
        "GET /provider",
        json!({
            "all": [{
                "id": "opencode",
                "name": "OpenCode",
                "models": {
                    "big-pickle": {
                        "id": "big-pickle",
                        "name": "Big Pickle",
                        "capabilities": {
                            "reasoning": false,
                            "input": {
                                "text": true,
                                "image": false
                            }
                        }
                    }
                }
            }]
        }),
    );
    let mut fx = bring_up_bridge("v11-model-provider", state.clone()).await;

    send(&mut fx.write, 2, "model/list", json!({})).await;
    let resp = read_until_response(&mut fx.read, 2).await;
    let data = resp["result"]["data"].as_array().expect("data");
    assert_eq!(data.len(), 1, "{data:#?}");
    assert_eq!(data[0]["id"], "opencode/big-pickle");
    assert_eq!(data[0]["model"], "opencode/big-pickle");
    assert_eq!(data[0]["displayName"], "OpenCode / OpenCode / Big Pickle");
    assert_eq!(data[0]["inputModalities"], json!(["text"]));
    assert_eq!(data[0]["isDefault"], true);
    assert_eq!(
        data[0]["supportedReasoningEfforts"],
        json!([{"reasoningEffort":"medium","description":""}])
    );

    fx.shutdown().await;
}

#[tokio::test]
async fn thread_loaded_list_returns_codex_list_shape() {
    let state = std::sync::Arc::new(std::sync::Mutex::new(FakeServerState::default()));
    let mut fx = bring_up_bridge("v11-loaded-list", state.clone()).await;

    send(&mut fx.write, 2, "thread/loaded/list", json!({})).await;
    let resp = read_until_response(&mut fx.read, 2).await;

    assert_eq!(resp["result"]["data"], json!([]));
    assert!(resp["result"]["nextCursor"].is_null());
    assert!(resp["result"].get("threadIds").is_none(), "{resp:#?}");

    fx.shutdown().await;
}
