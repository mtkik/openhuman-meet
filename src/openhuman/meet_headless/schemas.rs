//! Controller schemas for the `meet_headless` domain.

use serde_json::{Map, Value};

use crate::core::all::{ControllerFuture, RegisteredController};
use crate::core::{ControllerSchema, FieldSchema, TypeSchema};

type SchemaBuilder = fn() -> ControllerSchema;
type ControllerHandler = fn(Map<String, Value>) -> ControllerFuture;

struct Def {
    function: &'static str,
    schema: SchemaBuilder,
    handler: ControllerHandler,
}

const DEFS: &[Def] = &[
    Def {
        function: "start",
        schema: schema_start,
        handler: handle_start,
    },
    Def {
        function: "stop",
        schema: schema_stop,
        handler: handle_stop,
    },
];

pub fn all_controller_schemas() -> Vec<ControllerSchema> {
    DEFS.iter().map(|d| (d.schema)()).collect()
}

pub fn all_registered_controllers() -> Vec<RegisteredController> {
    DEFS.iter()
        .map(|d| RegisteredController {
            schema: (d.schema)(),
            handler: d.handler,
        })
        .collect()
}

fn schema_start() -> ControllerSchema {
    ControllerSchema {
        namespace: "meet_headless",
        function: "start",
        description:
            "Launch a headless chromium, join the Meet URL as a guest, and start \
             watching live captions. The session is keyed by request_id and feeds \
             captions into the matching meet_agent session.",
        inputs: vec![
            FieldSchema {
                name: "request_id",
                ty: TypeSchema::String,
                comment: "Caller-minted UUID. Used as the session key so push_caption / \
                          stop calls find it again.",
                required: true,
            },
            FieldSchema {
                name: "meet_url",
                ty: TypeSchema::String,
                comment: "Meet call URL (https://meet.google.com/<code>). Validated.",
                required: true,
            },
            FieldSchema {
                name: "display_name",
                ty: TypeSchema::String,
                comment: "Guest name typed into the 'Your name' input on the lobby \
                          page. Trimmed, length-capped to 64 chars.",
                required: true,
            },
        ],
        outputs: vec![
            FieldSchema {
                name: "ok",
                ty: TypeSchema::Bool,
                comment: "True when chromium booted and the session is live.",
                required: true,
            },
            FieldSchema {
                name: "request_id",
                ty: TypeSchema::String,
                comment: "Echoed session key.",
                required: true,
            },
            FieldSchema {
                name: "meet_url",
                ty: TypeSchema::String,
                comment: "Echoed (normalised) Meet URL.",
                required: true,
            },
        ],
    }
}

fn schema_stop() -> ControllerSchema {
    ControllerSchema {
        namespace: "meet_headless",
        function: "stop",
        description:
            "Stop a headless Meet session: shut down the caption watcher, kill the \
             chromium child process, and clean up the ephemeral profile dir.",
        inputs: vec![FieldSchema {
            name: "request_id",
            ty: TypeSchema::String,
            comment: "Session key from start.",
            required: true,
        }],
        outputs: vec![
            FieldSchema {
                name: "ok",
                ty: TypeSchema::Bool,
                comment: "True when the session existed and was stopped.",
                required: true,
            },
            FieldSchema {
                name: "request_id",
                ty: TypeSchema::String,
                comment: "Echoed session key.",
                required: true,
            },
            FieldSchema {
                name: "captions_seen",
                ty: TypeSchema::U64,
                comment: "Total caption rows the watcher forwarded into the \
                          meet_agent session.",
                required: true,
            },
        ],
    }
}

fn handle_start(p: Map<String, Value>) -> ControllerFuture {
    Box::pin(async move { super::rpc::handle_start(p).await })
}
fn handle_stop(p: Map<String, Value>) -> ControllerFuture {
    Box::pin(async move { super::rpc::handle_stop(p).await })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_handlers_match_schemas() {
        let schema_fns: Vec<_> = all_controller_schemas()
            .into_iter()
            .map(|s| s.function)
            .collect();
        let handler_fns: Vec<_> = all_registered_controllers()
            .into_iter()
            .map(|c| c.schema.function)
            .collect();
        assert_eq!(schema_fns, handler_fns);
        assert_eq!(schema_fns, vec!["start", "stop"]);
    }
}
