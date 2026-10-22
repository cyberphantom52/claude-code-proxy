use std::collections::BTreeMap;

use claude_code_proxy::anthropic::sse::parse_sse_events;
use claude_code_proxy::providers::codex::translate::{
    accumulate::accumulate_response,
    live_stream::LiveStreamTranslator,
    reducer::{UpstreamErrorKind, reduce_upstream_bytes},
    stream::translate_stream_bytes,
};
use serde_json::{Value, json};

fn upstream(trace: &[Value]) -> Vec<u8> {
    trace
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>()
        .into_bytes()
}

fn added(name: &str) -> Value {
    json!({"type":"response.output_item.added", "output_index":0,
        "item":{"type":"function_call", "call_id":"call_1", "name":name}})
}

fn delta(args: &str) -> Value {
    json!({"type":"response.function_call_arguments.delta", "output_index":0, "delta":args})
}

fn done(name: &str, args: &str) -> Value {
    json!({"type":"response.output_item.done", "output_index":0,
        "item":{"type":"function_call", "call_id":"call_1", "name":name, "arguments":args}})
}

fn terminal() -> Value {
    json!({"type":"response.completed", "response":{"id":"resp_1", "service_tier":"default",
        "usage":{"input_tokens":11,"output_tokens":7}}})
}

// Reconstruct the actual downstream SSE, rejecting duplicate starts/stops and
// non-object arguments instead of only checking that tool_use appears in bytes.
fn reconstruct(bytes: &[u8]) -> Value {
    let mut blocks = BTreeMap::<usize, (Value, String, bool)>::new();
    let mut result = json!({});
    let mut starts = 0;
    let mut stops = 0;
    for event in parse_sse_events(bytes) {
        let event: Value = serde_json::from_str(&event.data).unwrap();
        let index = event["index"].as_u64().unwrap_or(0) as usize;
        match event["type"].as_str().unwrap() {
            "message_start" => starts += 1,
            "content_block_start" => {
                assert!(
                    blocks
                        .insert(
                            index,
                            (event["content_block"].clone(), String::new(), false)
                        )
                        .is_none()
                );
            }
            "content_block_delta" => {
                let (block, args, closed) = blocks.get_mut(&index).unwrap();
                assert!(!*closed);
                let delta = &event["delta"];
                match delta["type"].as_str().unwrap() {
                    "input_json_delta" => args.push_str(delta["partial_json"].as_str().unwrap()),
                    "text_delta" => {
                        block["text"] = json!(format!(
                            "{}{}",
                            block["text"].as_str().unwrap(),
                            delta["text"].as_str().unwrap()
                        ))
                    }
                    "thinking_delta" => {
                        block["thinking"] = json!(format!(
                            "{}{}",
                            block["thinking"].as_str().unwrap(),
                            delta["thinking"].as_str().unwrap()
                        ))
                    }
                    "signature_delta" => block["signature"] = delta["signature"].clone(),
                    other => panic!("unexpected delta {other}"),
                }
            }
            "content_block_stop" => {
                let (_, _, closed) = blocks.get_mut(&index).unwrap();
                assert!(!*closed);
                *closed = true;
            }
            "message_delta" => {
                assert!(result.get("stop_reason").is_none());
                result["stop_reason"] = event["delta"]["stop_reason"].clone();
                result["usage"] = event["usage"].clone();
            }
            "message_stop" => stops += 1,
            "ping" => {}
            other => panic!("unexpected event {other}"),
        }
    }
    assert_eq!((starts, stops), (1, 1));
    result["content"] = Value::Array(
        blocks
            .into_values()
            .map(|(mut block, args, closed)| {
                assert!(closed);
                if block["type"] == "tool_use" {
                    let input: Value = serde_json::from_str(&args).unwrap();
                    assert!(input.is_object());
                    block["input"] = input;
                }
                block
            })
            .collect(),
    );
    result
}

fn assert_success(trace: &[Value], expected_content: Value) -> Value {
    let bytes = upstream(trace);
    let accumulated = accumulate_response(&bytes, "msg_1", "gpt-5.5-fast").unwrap();
    let buffered = reconstruct(&translate_stream_bytes(&bytes, "msg_1", "gpt-5.5-fast").unwrap());
    let mut live = LiveStreamTranslator::new("msg_1", "gpt-5.5-fast");
    let mut live_bytes = Vec::new();
    for event in trace {
        live_bytes.extend(live.accept(event, None).unwrap());
    }
    assert!(live.is_finished());
    let reconstructed = reconstruct(&live_bytes);
    for response in [&accumulated, &buffered, &reconstructed] {
        assert_eq!(response["content"], expected_content);
        assert_eq!(response["stop_reason"], "tool_use");
        assert_eq!(response["usage"], accumulated["usage"]);
    }
    accumulated
}

fn assert_rejected(trace: &[Value], expected_error: &str) {
    let bytes = upstream(trace);
    let reducer_error = reduce_upstream_bytes(&bytes).unwrap_err();
    assert!(
        reducer_error.message.contains(expected_error),
        "{}",
        reducer_error.message
    );
    assert!(accumulate_response(&bytes, "msg_1", "gpt-5.5").is_err());
    assert!(translate_stream_bytes(&bytes, "msg_1", "gpt-5.5").is_err());
    let mut live = LiveStreamTranslator::new("msg_1", "gpt-5.5");
    let mut output = Vec::new();
    let error = trace
        .iter()
        .find_map(|event| match live.accept(event, None) {
            Ok(bytes) => {
                output.extend(bytes);
                None
            }
            Err(error) => Some(error),
        })
        .expect("live translation must fail");
    assert!(error.contains(expected_error), "{error}");
    assert!(!live.is_finished());
    assert!(!String::from_utf8_lossy(&output).contains("event: message_stop"));
    if expected_error.contains("safe limits") {
        assert_eq!(reducer_error.kind, UpstreamErrorKind::Failed);
    }
}

#[test]
fn done_only_calls_are_validated_and_preserved_in_all_paths() {
    for name in ["Bash", "Read"] {
        for indexed in [false, true] {
            for args in ["{\"key\":1}", "", " \n\t"] {
                let mut done = done(name, args);
                if !indexed {
                    done.as_object_mut().unwrap().remove("output_index");
                }
                assert_success(
                    &[done, terminal()],
                    json!([{
                        "type":"tool_use", "id":"call_1", "name":name,
                        "input":if args.trim().is_empty() { json!({}) } else { json!({"key":1}) }
                    }]),
                );
            }
        }
    }
    for args in ["{", "[]", "null", "\"string\"", "1"] {
        assert_rejected(&[done("Bash", args), terminal()], "arguments");
    }
    for field in ["name", "call_id", "arguments"] {
        let mut item = done("Bash", "{}");
        item["item"].as_object_mut().unwrap().remove(field);
        assert_rejected(&[item, terminal()], "missing");
    }
}

#[test]
fn missing_tool_completion_never_becomes_an_executable_turn() {
    for name in ["Bash", "Read"] {
        for args in ["", "{", "[]", "{}"] {
            assert_rejected(&[added(name), delta(args), terminal()], "open Codex");
        }
        assert_rejected(
            &[added(name), delta(" \n\t"), done(name, "{}"), terminal()],
            "not valid JSON",
        );
        assert_rejected(
            &[added(name), delta("{"), done(name, "{"), terminal()],
            "not valid JSON",
        );
        assert_rejected(
            &[added(name), delta("[]"), done(name, "[]"), terminal()],
            "JSON object",
        );
    }
}

#[test]
fn authoritative_final_must_agree_with_streamed_identity_and_json() {
    for name in ["Bash", "Read"] {
        for partial in ["{\"key\":1}", "{\"key\":", "[]"] {
            assert_rejected(
                &[
                    added(name),
                    delta(partial),
                    done(name, "{\"key\":2}"),
                    terminal(),
                ],
                if partial == "[]" {
                    "JSON object"
                } else if partial.ends_with(':') {
                    "not valid JSON"
                } else {
                    "disagrees"
                },
            );
        }
        for field in ["name", "call_id"] {
            let mut final_item = done(name, "{}");
            final_item["item"][field] = json!("different");
            assert_rejected(
                &[added(name), delta("{}"), final_item, terminal()],
                "disagrees",
            );
        }
        for final_args in ["{", "[]", "null"] {
            assert_rejected(
                &[added(name), delta("{}"), done(name, final_args), terminal()],
                "arguments",
            );
        }
        assert_success(
            &[
                added(name),
                delta("{\"a\":1,"),
                delta("\"b\":2}"),
                done(name, "{ \"b\": 2, \"a\": 1 }"),
                terminal(),
            ],
            json!([{"type":"tool_use","id":"call_1","name":name,"input":{"a":1,"b":2}}]),
        );
    }
}

#[test]
fn arguments_done_is_validated_and_not_replayed_or_ignored() {
    let args_done = |args: &str| json!({"type":"response.function_call_arguments.done", "output_index":0, "arguments":args});
    assert_success(
        &[
            added("Bash"),
            args_done("{\"key\":1}"),
            done("Bash", "{\"key\":1}"),
            terminal(),
        ],
        json!([{"type":"tool_use","id":"call_1","name":"Bash","input":{"key":1}}]),
    );
    assert_rejected(
        &[
            added("Bash"),
            delta("{\"key\":1}"),
            args_done("{\"key\":2}"),
            done("Bash", "{\"key\":1}"),
            terminal(),
        ],
        "disagrees",
    );
    assert_rejected(
        &[
            added("Bash"),
            args_done("{\"key\":1}"),
            done("Bash", "{\"key\":2}"),
            terminal(),
        ],
        "disagrees",
    );
    for args in ["{", "[]"] {
        assert_rejected(
            &[
                added("Bash"),
                args_done(args),
                done("Bash", "{}"),
                terminal(),
            ],
            "arguments",
        );
    }
}

#[test]
fn read_repair_waits_for_finalization_and_preserves_sibling_failure_and_usage() {
    let stalled = format!(
        "{{\"file_path\":\"/tmp/a\",\"pages\":\"\"{}",
        " ".repeat(1024)
    );
    let prefix = vec![
        json!({"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"msg_text"}}),
        added("Read"),
        delta(&stalled),
        json!({"type":"response.output_text.delta","output_index":1,"delta":"sibling"}),
    ];
    let read_done = done("Read", "{\"file_path\":\"/tmp/a\",\"pages\":\"\"}");
    for close_read in [false, true] {
        let mut trace = prefix.clone();
        if close_read {
            trace.push(read_done.clone());
        }
        trace.push(json!({"type":"response.failed", "response":{"status":"failed","error":{"code":"server_error","message":"authoritative failure"}}}));
        assert_rejected(&trace, "authoritative failure");
    }
    let mut success = prefix;
    success.extend([
        read_done,
        json!({"type":"response.output_item.done","output_index":1,"item":{"type":"message"}}),
        terminal(),
    ]);
    let response = assert_success(
        &success,
        json!([
            {"type":"text","text":"sibling"},
            {"type":"tool_use","id":"call_1","name":"Read","input":{"file_path":"/tmp/a"}}
        ]),
    );
    assert_eq!(response["usage"]["output_tokens"], 7);
    assert_eq!(response["usage"]["speed"], "standard");
    let mut mismatch = success.clone();
    mismatch[4] = done("Read", "{\"file_path\":\"/tmp/other\"}");
    assert_rejected(&mismatch, "disagrees");
}

#[test]
fn all_tool_argument_sources_enforce_the_same_size_cap() {
    const LIMIT: usize = 5_000_000;
    let oversized = format!("{{\"x\":\"{}\"}}", "x".repeat(LIMIT - 7));
    assert_eq!(oversized.len(), LIMIT + 1);
    for name in ["Bash", "Read"] {
        let args_done = json!({"type":"response.function_call_arguments.done","output_index":0,"arguments":oversized});
        for trace in [
            vec![added(name), delta(&oversized), done(name, "{}"), terminal()],
            vec![added(name), args_done, done(name, "{}"), terminal()],
            vec![added(name), done(name, &oversized), terminal()],
            vec![done(name, &oversized), terminal()],
        ] {
            assert_rejected(&trace, "safe limits");
        }
    }
    let boundary = format!("{{\"x\":\"{}\"}}", "x".repeat(LIMIT - 8));
    assert_eq!(boundary.len(), LIMIT);
    let item =
        json!({"type":"tool_use","id":"call_1","name":"Bash","input":{"x":"x".repeat(LIMIT - 8)}});
    assert_success(
        &[
            added("Bash"),
            delta(&boundary),
            done("Bash", &boundary),
            terminal(),
        ],
        json!([item]),
    );
}

#[test]
fn validated_tool_turn_preserves_reasoning_and_supported_terminal_forms() {
    use claude_code_proxy::providers::codex::translate::reasoning_signature::{
        ReasoningReplay, encode_reasoning_signature,
    };
    let signature = encode_reasoning_signature(&ReasoningReplay {
        id: "rs_1".into(),
        encrypted_content: "opaque-encrypted-content".into(),
    })
    .unwrap();
    for kind in ["response.completed", "response.done"] {
        let mut completion = terminal();
        completion["type"] = json!(kind);
        let response = assert_success(
            &[
                json!({"type":"response.output_item.added","output_index":1,
                "item":{"type":"reasoning","id":"rs_1","encrypted_content":"opaque-encrypted-content"}}),
                json!({"type":"response.reasoning_summary_text.delta","output_index":1,"delta":"plan"}),
                json!({"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","id":"rs_1"}}),
                added("Bash"),
                delta("{}"),
                done("Bash", "{}"),
                completion,
            ],
            json!([
                {"type":"thinking","thinking":"plan","signature":signature},
                {"type":"tool_use","id":"call_1","name":"Bash","input":{}}
            ]),
        );
        assert_eq!(response["usage"]["speed"], "standard");
    }
}

#[test]
fn done_only_sibling_does_not_close_or_replay_an_open_call() {
    let mut sibling = done("Bash", "{\"sibling\":true}");
    sibling["item"]["call_id"] = json!("call_2");
    sibling.as_object_mut().unwrap().remove("output_index");
    assert_success(
        &[
            added("Bash"),
            delta("{}"),
            sibling,
            done("Bash", "{}"),
            terminal(),
        ],
        json!([
            {"type":"tool_use","id":"call_1","name":"Bash","input":{}},
            {"type":"tool_use","id":"call_2","name":"Bash","input":{"sibling":true}}
        ]),
    );
}
