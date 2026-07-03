use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use crate::ck_wire::{
    CkIngressMessage, CkKind, CkOutputKind, CkToolOutput, CkWireBlock, CkWireMessage, HarnessMeta,
    MediaBlock, MediaKind, MessageOrigin, OpaqueBlock, ProviderExtras, ResultBlock,
    ResultBlockKind,
};

use super::sidecar::{
    meta_for_ck, stable_hash_prefix, BlockMeta, DecodeSidecar, DecodedHarnessMessages,
    ExtractedBoundary, HarnessMessageMeta,
};

pub type MessageV2Json = Value;

const HARNESS: &str = "opencode";

pub fn decode_opencode(messages: &[MessageV2Json]) -> DecodedHarnessMessages {
    decode_opencode_with_sidecar(messages, None)
}

pub fn decode_opencode_with_sidecar(
    messages: &[MessageV2Json],
    prior: Option<&DecodeSidecar>,
) -> DecodedHarnessMessages {
    let mut sidecar = DecodeSidecar::new(HARNESS);
    if let Some(prior) = prior {
        sidecar.mid_pins = prior.mid_pins.clone();
    }

    let mut decoded = Vec::with_capacity(messages.len());
    let mut boundary = None;

    for (message_index, raw_message) in messages.iter().enumerate() {
        let ordinal = (message_index + 1) as u64;
        let info = raw_message.get("info").unwrap_or(raw_message);
        let stable_key = string_field(info, "id")
            .or_else(|| string_field(raw_message, "id"))
            .unwrap_or_else(|| format!("opencode-hash-{}", stable_hash_prefix(raw_message, 24)));
        let mid = sidecar
            .inherit_pin(&stable_key)
            .unwrap_or_else(|| stable_key.clone());
        sidecar.pin_mid(stable_key.clone(), mid.clone());

        let role = string_field(info, "role")
            .or_else(|| string_field(raw_message, "role"))
            .unwrap_or_else(|| "user".to_string());
        let origin = opencode_origin(info).or_else(|| opencode_origin(raw_message));
        let parts = raw_message
            .get("parts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut content = Vec::new();
        let mut block_metas = Vec::new();

        for (part_index, part) in parts.iter().enumerate() {
            let part_type = string_field(part, "type").unwrap_or_else(|| "unknown".to_string());
            match part_type.as_str() {
                "text" => {
                    if part
                        .get("ignored")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    let text = string_field(part, "text").unwrap_or_default();
                    let block =
                        block_with_metadata(CkKind::Text { text }, part.get("metadata").cloned());
                    push_block(
                        &mut content,
                        &mut block_metas,
                        block,
                        part_index,
                        part,
                        "text",
                    );
                }
                "reasoning" => {
                    let text = string_field(part, "text")
                        .or_else(|| string_field(part, "thinking"))
                        .unwrap_or_default();
                    let metadata = part.get("metadata").cloned();
                    let kind = if text.is_empty() {
                        if let Some(data) = redacted_reasoning_data(part) {
                            CkKind::RedactedReasoning { data }
                        } else {
                            CkKind::Reasoning {
                                text,
                                signature: metadata.as_ref().and_then(find_signature),
                            }
                        }
                    } else {
                        CkKind::Reasoning {
                            text,
                            signature: metadata.as_ref().and_then(find_signature),
                        }
                    };
                    let block = block_with_metadata(kind, metadata);
                    push_block(
                        &mut content,
                        &mut block_metas,
                        block,
                        part_index,
                        part,
                        "reasoning",
                    );
                }
                "tool" => {
                    decode_tool_part(ordinal, part_index, part, &mut content, &mut block_metas);
                }
                "file" | "image" => {
                    let media = media_from_part(part);
                    let block = CkWireBlock::bare(CkKind::Media(media));
                    push_block(
                        &mut content,
                        &mut block_metas,
                        block,
                        part_index,
                        part,
                        "file",
                    );
                }
                "step-start" => {
                    let block = opaque_block("step-start", part.clone(), None);
                    push_block(
                        &mut content,
                        &mut block_metas,
                        block,
                        part_index,
                        part,
                        "step-start",
                    );
                }
                "compaction" => {
                    boundary = Some(ExtractedBoundary {
                        harness: HARNESS.to_string(),
                        message_id: mid.clone(),
                        ordinal,
                        part_index: Some(part_index),
                        entry_id: None,
                        raw: part.clone(),
                    });
                }
                "subtask" => {
                    let block = opaque_block("subtask", part.clone(), None);
                    push_block(
                        &mut content,
                        &mut block_metas,
                        block,
                        part_index,
                        part,
                        "subtask",
                    );
                }
                "step-finish" | "snapshot" | "patch" | "agent" | "retry" => {}
                _ => {
                    let block = opaque_block(&part_type, part.clone(), opaque_arc(part));
                    push_block(
                        &mut content,
                        &mut block_metas,
                        block,
                        part_index,
                        part,
                        &part_type,
                    );
                }
            }
        }

        let ck = CkWireMessage::from_parts(
            role.clone(),
            content,
            origin,
            ProviderExtras::new(),
            HarnessMeta {
                harness_id: Some(mid.clone()),
                ordinal: Some(ordinal),
                synthetic: false,
            },
        );
        decoded.push(CkIngressMessage {
            mid: mid.clone(),
            ordinal,
            ck,
        });
        sidecar.remember_message(
            mid.clone(),
            HarnessMessageMeta {
                mid,
                ordinal,
                role,
                raw: raw_message.clone(),
                stable_key: Some(stable_key),
                blocks: block_metas,
            },
        );
    }

    DecodedHarnessMessages {
        messages: decoded,
        boundary,
        sidecar,
    }
}

pub fn encode_opencode(messages: &[CkWireMessage], sidecar: &DecodeSidecar) -> Vec<MessageV2Json> {
    messages
        .iter()
        .enumerate()
        .map(|(index, msg)| match meta_for_ck(sidecar, msg, index) {
            Some(meta) => encode_with_meta(msg, meta),
            None => encode_new_message(msg),
        })
        .collect()
}

fn decode_tool_part(
    ordinal: u64,
    part_index: usize,
    part: &Value,
    content: &mut Vec<CkWireBlock>,
    block_metas: &mut Vec<BlockMeta>,
) {
    let tool_name = tool_name(part);
    let input = part
        .get("state")
        .and_then(|state| state.get("input"))
        .or_else(|| part.get("input"))
        .or_else(|| part.get("args"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let id = string_field(part, "callID")
        .or_else(|| string_field(part, "callId"))
        .or_else(|| string_field(part, "id"))
        .unwrap_or_else(|| synth_tool_id(ordinal, part_index, &tool_name, &input));
    let provider_executed = part
        .get("metadata")
        .and_then(|m| m.get("providerExecuted"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let call_block = CkWireBlock::bare(CkKind::ToolCall {
        id: id.clone(),
        name: tool_name.clone(),
        input,
        provider_executed,
    });
    push_block(
        content,
        block_metas,
        call_block,
        part_index,
        part,
        "tool_call",
    );

    let status = tool_status(part);
    if matches!(status.as_deref(), Some("completed" | "error")) {
        let output_text = part
            .get("state")
            .and_then(|state| state.get("output"))
            .or_else(|| part.get("output"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let output = tool_output_from_part(part, status.as_deref() == Some("error"), output_text);
        let result_block = CkWireBlock::bare(CkKind::ToolResult {
            id,
            tool_name,
            output,
            provider_executed,
        });
        push_block(
            content,
            block_metas,
            result_block,
            part_index,
            part,
            "tool_result",
        );
    }
}

fn push_block(
    content: &mut Vec<CkWireBlock>,
    block_metas: &mut Vec<BlockMeta>,
    block: CkWireBlock,
    part_index: usize,
    raw: &Value,
    kind: &str,
) {
    let block_index = content.len();
    content.push(block);
    block_metas.push(BlockMeta {
        block_index,
        kind: kind.to_string(),
        native_index: Some(part_index),
        native_id: string_field(raw, "id").or_else(|| string_field(raw, "callID")),
        item_id: None,
        raw: raw.clone(),
    });
}

fn block_with_metadata(kind: CkKind, metadata: Option<Value>) -> CkWireBlock {
    if let Some(metadata) = metadata {
        let mut extras = ProviderExtras::new();
        let mut ns = BTreeMap::new();
        ns.insert("metadata".to_string(), metadata);
        extras.insert(HARNESS.to_string(), ns);
        CkWireBlock::with_provider_extras(kind, extras)
    } else {
        CkWireBlock::bare(kind)
    }
}

fn opencode_origin(value: &Value) -> Option<MessageOrigin> {
    let model = value.get("model").unwrap_or(value);
    let provider = string_field(model, "providerID")
        .or_else(|| string_field(model, "provider"))
        .or_else(|| string_field(value, "providerID"))
        .or_else(|| string_field(value, "provider"))?;
    let model_id = string_field(model, "modelID")
        .or_else(|| string_field(model, "model"))
        .or_else(|| string_field(value, "modelID"))
        .or_else(|| string_field(value, "model"))?;
    Some(MessageOrigin {
        api: provider.clone(),
        provider,
        model: model_id,
    })
}

fn media_from_part(part: &Value) -> MediaBlock {
    let media_type = string_field(part, "mime")
        .or_else(|| string_field(part, "mimeType"))
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let filename = string_field(part, "filename").or_else(|| string_field(part, "name"));
    let source = media_source(part, &media_type);
    MediaBlock {
        kind: media_kind(&media_type),
        media_type,
        filename,
        source,
    }
}

fn media_source(part: &Value, media_type: &str) -> Value {
    if let Some(data) = string_field(part, "data") {
        return json!({ "type": "data_base64", "data": data });
    }
    if let Some(url) = string_field(part, "url") {
        let prefix = format!("data:{media_type};base64,");
        if let Some(data) = url.strip_prefix(&prefix) {
            return json!({ "type": "data_base64", "data": data });
        }
        return json!({ "type": "url", "url": url });
    }
    json!({ "type": "opaque", "raw": part })
}

fn media_kind(media_type: &str) -> MediaKind {
    if media_type.starts_with("image/") {
        MediaKind::Image
    } else if media_type.starts_with("audio/") {
        MediaKind::Audio
    } else if media_type.starts_with("video/") {
        MediaKind::Video
    } else if media_type == "application/pdf" {
        MediaKind::Document
    } else {
        MediaKind::File
    }
}

fn tool_output_from_part(part: &Value, is_error: bool, output_text: String) -> CkToolOutput {
    let attachments = part
        .get("state")
        .and_then(|state| state.get("attachments"))
        .or_else(|| part.get("attachments"))
        .and_then(Value::as_array);
    let Some(attachments) = attachments else {
        return if is_error {
            CkToolOutput::bare(CkOutputKind::ErrorText { text: output_text })
        } else {
            CkToolOutput::bare(CkOutputKind::Text { text: output_text })
        };
    };

    let mut blocks = Vec::new();
    if !output_text.is_empty() {
        blocks.push(ResultBlock {
            kind: ResultBlockKind::Text { text: output_text },
            provider_extras: ProviderExtras::new(),
        });
    }
    for attachment in attachments {
        if attachment.is_object() {
            blocks.push(ResultBlock {
                kind: ResultBlockKind::Media {
                    media: media_from_part(attachment),
                },
                provider_extras: ProviderExtras::new(),
            });
        }
    }
    CkToolOutput::bare(CkOutputKind::Content { blocks })
}

fn encode_with_meta(msg: &CkWireMessage, meta: &HarnessMessageMeta) -> Value {
    let mut raw = meta.raw.clone();
    let mut parts = raw
        .get("parts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let block_meta_by_index: BTreeMap<usize, &BlockMeta> = meta
        .blocks
        .iter()
        .map(|block_meta| (block_meta.block_index, block_meta))
        .collect();

    for (block_index, block) in msg.content.iter().enumerate() {
        if let Some(block_meta) = block_meta_by_index.get(&block_index) {
            if let Some(part_index) = block_meta.native_index {
                if let Some(part) = parts.get_mut(part_index) {
                    update_part_from_block(part, block);
                    continue;
                }
            }
        }
        parts.push(render_block_as_part(block));
    }

    parts.retain(|part| part.get("type").and_then(Value::as_str) != Some("compaction"));

    if let Some(obj) = raw.as_object_mut() {
        obj.insert("parts".to_string(), Value::Array(parts));
        let info = obj.entry("info").or_insert_with(|| json!({}));
        if let Some(info_obj) = info.as_object_mut() {
            info_obj
                .entry("id".to_string())
                .or_insert_with(|| Value::String(meta.mid.clone()));
            info_obj.insert("role".to_string(), Value::String(msg.role.clone()));
        }
    } else {
        raw = json!({
            "info": { "id": meta.mid, "role": msg.role },
            "parts": parts,
        });
    }
    raw
}

fn update_part_from_block(part: &mut Value, block: &CkWireBlock) {
    match &block.kind {
        CkKind::Text { text } => {
            set_string(part, "type", "text");
            set_string(part, "text", text);
            if let Some(metadata) = block
                .provider_extras
                .get(HARNESS)
                .and_then(|ns| ns.get("metadata"))
            {
                set_value(part, "metadata", metadata.clone());
            }
        }
        CkKind::Reasoning { text, signature } => {
            set_string(part, "type", "reasoning");
            set_string(part, "text", text);
            if part.get("metadata").is_none() {
                if let Some(signature) = signature {
                    set_value(part, "metadata", json!({ "signature": signature }));
                }
            }
        }
        CkKind::RedactedReasoning { data } => {
            set_string(part, "type", "reasoning");
            set_string(part, "text", "");
            if part.get("metadata").is_none() {
                set_value(part, "metadata", json!({ "redacted": data }));
            }
        }
        CkKind::ToolCall {
            id,
            name,
            input,
            provider_executed,
        } => {
            set_string(part, "type", "tool");
            set_string(part, "callID", id);
            set_string(part, "tool", name);
            set_nested_value(part, "state", "input", input.clone());
            if *provider_executed {
                set_nested_value(part, "metadata", "providerExecuted", Value::Bool(true));
            }
        }
        CkKind::ToolResult { output, .. } => {
            let (status, text) = output_status_text(output);
            set_string(part, "type", "tool");
            set_nested_value(part, "state", "status", Value::String(status.to_string()));
            set_nested_value(part, "state", "output", Value::String(text));
        }
        CkKind::Media(media) => {
            *part = render_media_part(media);
        }
        CkKind::Opaque(opaque) => {
            *part = opaque.raw.clone();
        }
    }
}

fn encode_new_message(msg: &CkWireMessage) -> Value {
    let id = msg
        .meta
        .harness_id
        .clone()
        .unwrap_or_else(|| format!("opencode-ck-{}", stable_hash_prefix(&json!(msg.role), 12)));
    let mut parts = Vec::new();
    let mut index = 0;
    while index < msg.content.len() {
        let block = &msg.content[index];
        if let CkKind::ToolCall { id, .. } = &block.kind {
            if let Some(next) = msg.content.get(index + 1) {
                if matches!(&next.kind, CkKind::ToolResult { id: result_id, .. } if result_id == id)
                {
                    parts.push(render_tool_pair_as_part(block, next));
                    index += 2;
                    continue;
                }
            }
        }
        parts.push(render_block_as_part(block));
        index += 1;
    }
    json!({
        "info": { "id": id, "role": msg.role },
        "parts": parts,
    })
}

fn render_block_as_part(block: &CkWireBlock) -> Value {
    match &block.kind {
        CkKind::Text { text } => json!({ "type": "text", "text": text }),
        CkKind::Reasoning { text, signature } => {
            let mut part = json!({ "type": "reasoning", "text": text });
            if let Some(signature) = signature {
                set_value(&mut part, "metadata", json!({ "signature": signature }));
            }
            part
        }
        CkKind::RedactedReasoning { data } => {
            json!({ "type": "reasoning", "text": "", "metadata": { "redacted": data } })
        }
        CkKind::ToolCall {
            id,
            name,
            input,
            provider_executed,
        } => {
            let mut part = json!({
                "type": "tool",
                "callID": id,
                "tool": name,
                "state": { "status": "running", "input": input },
            });
            if *provider_executed {
                set_nested_value(&mut part, "metadata", "providerExecuted", Value::Bool(true));
            }
            part
        }
        CkKind::ToolResult { output, .. } => {
            let (status, text) = output_status_text(output);
            json!({ "type": "tool", "state": { "status": status, "output": text } })
        }
        CkKind::Media(media) => render_media_part(media),
        CkKind::Opaque(opaque) => opaque.raw.clone(),
    }
}

fn render_tool_pair_as_part(call: &CkWireBlock, result: &CkWireBlock) -> Value {
    let mut part = render_block_as_part(call);
    update_part_from_block(&mut part, result);
    part
}

fn render_media_part(media: &MediaBlock) -> Value {
    let mut part = json!({
        "type": "file",
        "mime": media.media_type,
    });
    if let Some(filename) = &media.filename {
        set_string(&mut part, "filename", filename);
    }
    if let Some(obj) = media.source.as_object() {
        match obj.get("type").and_then(Value::as_str) {
            Some("data_base64") => {
                if let Some(data) = obj.get("data").and_then(Value::as_str) {
                    set_string(
                        &mut part,
                        "url",
                        &format!("data:{};base64,{data}", media.media_type),
                    );
                }
            }
            Some("url") => {
                if let Some(url) = obj.get("url").and_then(Value::as_str) {
                    set_string(&mut part, "url", url);
                }
            }
            _ => {}
        }
    }
    part
}

fn output_status_text(output: &CkToolOutput) -> (&'static str, String) {
    match &output.kind {
        CkOutputKind::Text { text } => ("completed", text.clone()),
        CkOutputKind::Json { value } => ("completed", value.to_string()),
        CkOutputKind::ErrorText { text } => ("error", text.clone()),
        CkOutputKind::ErrorJson { value } => ("error", value.to_string()),
        CkOutputKind::ExecutionDenied { reason } => (
            "error",
            reason
                .clone()
                .unwrap_or_else(|| "Execution denied".to_string()),
        ),
        CkOutputKind::Content { blocks } => {
            let text = blocks
                .iter()
                .filter_map(|block| match &block.kind {
                    ResultBlockKind::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            ("completed", text)
        }
    }
}

fn opaque_block(kind: &str, raw: Value, arc: Option<Value>) -> CkWireBlock {
    CkWireBlock::bare(CkKind::Opaque(OpaqueBlock {
        source: json!({ "type": "harness", "harness": HARNESS }),
        kind: kind.to_string(),
        raw,
        arc,
    }))
}

fn opaque_arc(part: &Value) -> Option<Value> {
    let approval_id = string_field(part, "approvalId")?;
    let part_type = string_field(part, "type").unwrap_or_default();
    let role = if part_type.contains("response") {
        "Response"
    } else {
        "Request"
    };
    Some(json!({ "kind": "Approval", "id": approval_id, "role": role }))
}

fn redacted_reasoning_data(part: &Value) -> Option<String> {
    string_field(part, "data")
        .or_else(|| string_field(part, "redacted"))
        .or_else(|| {
            part.get("metadata")
                .and_then(|m| string_field(m, "redacted"))
        })
}

fn find_signature(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => {
            if let Some(signature) = map.get("signature").and_then(Value::as_str) {
                return Some(signature.to_string());
            }
            map.values().find_map(find_signature)
        }
        Value::Array(values) => values.iter().find_map(find_signature),
        _ => None,
    }
}

fn synth_tool_id(ordinal: u64, part_index: usize, tool_name: &str, input: &Value) -> String {
    format!(
        "synth-tool-{ordinal}-{part_index}-{tool_name}-{}",
        stable_hash_prefix(input, 12)
    )
}

fn tool_name(part: &Value) -> String {
    string_field(part, "tool")
        .or_else(|| string_field(part, "toolName"))
        .or_else(|| string_field(part, "name"))
        .unwrap_or_else(|| "tool".to_string())
}

fn tool_status(part: &Value) -> Option<String> {
    part.get("state")
        .and_then(|state| string_field(state, "status"))
        .or_else(|| string_field(part, "status"))
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn set_string(value: &mut Value, key: &str, text: &str) {
    set_value(value, key, Value::String(text.to_string()));
}

fn set_value(value: &mut Value, key: &str, next: Value) {
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    if let Some(obj) = value.as_object_mut() {
        obj.insert(key.to_string(), next);
    }
}

fn set_nested_value(value: &mut Value, object_key: &str, key: &str, next: Value) {
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    let entry = obj
        .entry(object_key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(Map::new());
    }
    if let Some(nested) = entry.as_object_mut() {
        nested.insert(key.to_string(), next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_and_ignored_text_obey_wire_reachability() {
        let raw = vec![json!({
            "info": { "id": "msg_1", "role": "user" },
            "parts": [
                { "type": "text", "text": "", "time": { "start": 1 } },
                { "type": "text", "text": "hidden", "ignored": true }
            ]
        })];
        let decoded = decode_opencode(&raw);
        assert_eq!(decoded.messages[0].ck.content.len(), 1);
        assert!(matches!(
            decoded.messages[0].ck.content[0].kind,
            CkKind::Text { ref text } if text.is_empty()
        ));
        assert_eq!(
            encode_opencode(&[decoded.messages[0].ck.clone()], &decoded.sidecar),
            raw
        );
    }

    #[test]
    fn compaction_is_extracted_as_boundary_not_content() {
        let raw = vec![json!({
            "info": { "id": "msg_boundary", "role": "user" },
            "parts": [
                { "type": "text", "text": "before" },
                { "type": "compaction", "auto": true }
            ]
        })];
        let decoded = decode_opencode(&raw);
        assert_eq!(decoded.messages[0].ck.content.len(), 1);
        assert_eq!(
            decoded.boundary.as_ref().unwrap().message_id,
            "msg_boundary"
        );
        assert_eq!(decoded.boundary.as_ref().unwrap().part_index, Some(1));
        let encoded = encode_opencode(&[decoded.messages[0].ck.clone()], &decoded.sidecar);
        let encoded_parts = encoded[0].get("parts").and_then(Value::as_array).unwrap();
        assert!(encoded_parts
            .iter()
            .all(|part| part.get("type").and_then(Value::as_str) != Some("compaction")));
    }
}
