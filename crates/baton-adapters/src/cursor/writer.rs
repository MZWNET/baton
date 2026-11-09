// Builds the agent graph of a chat: the model's history as JSON messages, with turns for the UI and rewind anchors as Cursor Agent records them

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use baton_core::{Blobs, Body, Event, Image, ToolResult, time};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::support::protobuf::Encoder;
use crate::support::unit::{self, Unit};
use crate::support::{identifiers, images, uri};

// A seeded history keeps Cursor from adding its own system prompt, so the chat brings a short one
const SYSTEM: &str = "You are a coding agent. This conversation was imported from another coding agent, and its earlier tool calls have already run. The workspace is";
// What the user says first when the history starts with the assistant, which providers reject
const CONTINUE: &str = "Continue the imported conversation.";
// `AgentMode.AGENT`
const AGENT: u64 = 1;

pub(super) struct Graph {
    // By their lowercase hex SHA-256
    pub(super) blobs: BTreeMap<String, Vec<u8>>,
    pub(super) root: String,
}

pub(super) fn write(events: &[&Event], cwd: &str, created: i64, blobs: &Blobs) -> Result<Graph> {
    let mut writer = GraphWriter {
        blobs,
        store: BTreeMap::new(),
        prompts: Vec::new(),
        turns: Vec::new(),
        archives: Vec::new(),
        turn: None,
        names: HashMap::new(),
        workspace: uri::from_path(cwd),
        created,
    };
    let system = writer.json(&json!({"role": "system", "content": format!("{SYSTEM} {cwd}.")}));
    writer.prompts.push(system);
    for unit in unit::units(events) {
        match unit {
            Unit::User(event, user) => writer.user(event, &user.text, &user.images, false)?,
            Unit::Assistant(run) => {
                writer.open(run[0])?;
                writer.assistant(&run);
            }
            Unit::ToolResult(event, result) => {
                writer.open(event)?;
                writer.result(result)?;
            }
            Unit::Compaction(_, summary) => writer.compaction(summary),
        }
    }
    writer.close();
    let root = writer.put(writer.state().finish());
    Ok(Graph {
        blobs: writer.store,
        root: base16ct::lower::encode_string(&root),
    })
}

// A turn still taking steps: its `UserMessage`, the request id that ties it to its prompt message, and its steps
struct Turn {
    user: Vec<u8>,
    request: String,
    steps: Vec<Vec<u8>>,
}

// Blob ids are raw SHA-256 digests, which is how the graph refers to blobs
struct GraphWriter<'a> {
    blobs: &'a Blobs,
    store: BTreeMap<String, Vec<u8>>,
    prompts: Vec<Vec<u8>>,
    turns: Vec<Vec<u8>>,
    archives: Vec<Vec<u8>>,
    turn: Option<Turn>,
    // Tool results name the tool of their call
    names: HashMap<String, String>,
    workspace: String,
    created: i64,
}

impl GraphWriter<'_> {
    fn put(&mut self, bytes: Vec<u8>) -> Vec<u8> {
        let id = Sha256::digest(&bytes).to_vec();
        self.store
            .insert(base16ct::lower::encode_string(&id), bytes);
        id
    }

    fn json(&mut self, message: &Value) -> Vec<u8> {
        self.put(message.to_string().into_bytes())
    }

    // A `ConversationStateStructure` of everything written so far, the root once the chat is complete and a rewind anchor before that
    fn state(&self) -> Encoder {
        let mut state = Encoder::default();
        for prompt in &self.prompts {
            state = state.bytes(1, prompt);
        }
        for turn in &self.turns {
            state = state.bytes(8, turn);
        }
        state = state.string(9, &self.workspace).varint(10, AGENT);
        for archive in &self.archives {
            state = state.bytes(13, archive);
        }
        state
            .string(22, "cli")
            .varint(26, self.created as u64)
            .string(27, "UTC")
    }

    fn open(&mut self, event: &Event) -> Result<()> {
        if self.turn.is_none() {
            self.user(event, CONTINUE, &[], true)?;
        }
        Ok(())
    }

    // The text goes in a query tag like the ones Cursor writes, so reading it back unwraps it exactly; baton marks a message it made up
    fn user(&mut self, event: &Event, text: &str, images: &[Image], synthetic: bool) -> Result<()> {
        self.close();
        let millis = time::unix_millis(&event.timestamp) as u64;
        let message_id = identifiers::uuid_v4();
        let request = identifiers::uuid_v4();
        let anchor = self.put(self.state().finish());
        let user = Encoder::default()
            .string(1, text)
            .string(2, &message_id)
            .bytes(3, &[])
            .varint(4, AGENT)
            .bytes(10, &anchor)
            .string(17, &message_id)
            .varint(25, millis)
            .varint(26, millis)
            .finish();
        let user = self.put(user);
        let mut content =
            vec![json!({"type": "text", "text": format!("<user_query>\n{text}\n</user_query>")})];
        for image in images {
            let data = images::encode(&self.blobs.get(&image.blob)?);
            content.push(json!({"type": "image", "image": data, "mimeType": image.mime}));
        }
        let mut options = json!({"cursor": {"requestId": request}});
        if synthetic {
            options["baton"] = json!({"synthetic": true});
        }
        let prompt = self.json(&json!({"role": "user", "content": content, "id": message_id, "providerOptions": options}));
        self.prompts.push(prompt);
        self.turn = Some(Turn {
            user,
            request,
            steps: Vec::new(),
        });
        Ok(())
    }

    // Tool calls get no step of their own: each tool has its own protobuf payload, and the model only sees the JSON
    fn assistant(&mut self, run: &[&Event]) {
        let mut content = Vec::new();
        for event in run {
            match &event.body {
                Body::Assistant(assistant) => {
                    content.push(json!({"type": "text", "text": assistant.text}));
                    let millis = time::unix_millis(&event.timestamp) as u64;
                    let reply = Encoder::default()
                        .string(1, &assistant.text)
                        .varint(2, millis)
                        .varint(3, millis);
                    let step = self.put(Encoder::default().message(1, reply).finish());
                    self.turn
                        .as_mut()
                        .expect("an assistant run follows a user message")
                        .steps
                        .push(step);
                }
                Body::ToolCall(call) => {
                    self.names.insert(call.call_id.clone(), call.name.clone());
                    content.push(json!({"type": "tool-call", "toolCallId": call.call_id, "toolName": call.name, "args": call.input}));
                }
                _ => {}
            }
        }
        // Messages with the same content would otherwise be the same blob
        let message = self
            .json(&json!({"role": "assistant", "content": content, "id": identifiers::uuid_v4()}));
        self.prompts.push(message);
    }

    fn result(&mut self, result: &ToolResult) -> Result<()> {
        let mut content = Vec::new();
        if !result.text.is_empty() {
            content.push(json!({"type": "text", "text": result.text}));
        }
        for image in &result.images {
            let data = images::encode(&self.blobs.get(&image.blob)?);
            content.push(json!({"type": "image", "data": data, "mimeType": image.mime}));
        }
        let name = self
            .names
            .get(&result.call_id)
            .map_or("unknown", String::as_str);
        let mut part = json!({"type": "tool-result", "toolCallId": result.call_id, "toolName": name, "result": result.text, "experimental_content": content});
        if result.is_error {
            part["isError"] = json!(true);
        }
        let message = self.json(&json!({"role": "tool", "content": [part], "id": result.call_id}));
        self.prompts.push(message);
        Ok(())
    }

    // The summary replaces the history for the model, and an archive keeps what it replaced
    fn compaction(&mut self, summary: &str) {
        let message = self.json(&json!({"role": "user", "content": [{"type": "text", "text": summary}], "providerOptions": {"cursor": {"isSummary": true}}}));
        let mut archive = Encoder::default();
        for prompt in &self.prompts[1..] {
            archive = archive.bytes(1, prompt);
        }
        let archive = self.put(archive.string(2, summary).bytes(4, &message).finish());
        self.archives.push(archive);
        self.prompts.truncate(1);
        self.prompts.push(message);
    }

    fn close(&mut self) {
        let Some(turn) = self.turn.take() else {
            return;
        };
        let mut agent = Encoder::default().bytes(1, &turn.user);
        for step in &turn.steps {
            agent = agent.bytes(2, step);
        }
        let id = self.put(
            Encoder::default()
                .message(1, agent.string(3, &turn.request))
                .finish(),
        );
        self.turns.push(id);
    }
}
