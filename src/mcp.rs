//! Minimal MCP server (Streamable HTTP, JSON responses) so an AI client can use Vigía as tools.
//! POST /mcp with the same Bearer token as the REST API.

use crate::{ApiResult, Ctx, NewWatch};
use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

fn tools() -> Value {
    let id = json!({ "type": "object", "properties": { "id": { "type": "string", "description": "Watch id" } }, "required": ["id"] });
    json!([
        { "name": "list_watches", "description": "List every watched page with its current price, stock, status and last change.", "inputSchema": { "type": "object", "properties": {} } },
        { "name": "add_watch", "description": "Start watching a URL. Runs the first check immediately and returns the watch (price and stock if the page is a product). Returns the existing watch if the URL is already watched.",
          "inputSchema": { "type": "object", "required": ["url"], "properties": {
            "url": { "type": "string" },
            "tag": { "type": "string", "description": "Optional label to group watches" },
            "interval": { "type": "integer", "description": "Seconds between checks (min 60, default 3600)" },
            "selector": { "type": "string", "description": "Optional CSS selector: only watch the text of that part of the page" },
            "mode": { "type": "string", "enum": ["auto", "price", "text"] } } } },
        { "name": "get_watch", "description": "One watch with its history of changes (price points or text versions).", "inputSchema": id },
        { "name": "check_watch", "description": "Check a watch right now. Returns whether it changed and its current state.", "inputSchema": id },
        { "name": "remove_watch", "description": "Stop watching and delete its history.", "inputSchema": id },
        { "name": "extract", "description": "Fetch any URL once, without saving it: title, price, currency, stock and visible text.",
          "inputSchema": { "type": "object", "required": ["url"], "properties": { "url": { "type": "string" }, "selector": { "type": "string" } } } }
    ])
}

async fn call(app: &Ctx, name: &str, a: &Value) -> ApiResult {
    let s = |k: &str| a.get(k).and_then(Value::as_str);
    let id = || s("id").ok_or(crate::ApiError(StatusCode::BAD_REQUEST, "id is required".into()));
    match name {
        "list_watches" => crate::list_watches(app),
        "add_watch" => {
            let n: NewWatch = serde_json::from_value(a.clone()).map_err(|e| crate::ApiError(StatusCode::BAD_REQUEST, e.to_string()))?;
            crate::add_watch(app, n).await
        }
        "get_watch" => crate::get_watch(app, id()?),
        "check_watch" => crate::check(app, id()?).await,
        "remove_watch" => crate::delete_watch(app, id()?),
        "extract" => crate::extract_once(app, s("url").unwrap_or_default(), s("selector")).await,
        other => Err(crate::ApiError(StatusCode::NOT_FOUND, format!("unknown tool {other}"))),
    }
}

pub async fn handle(State(app): State<Ctx>, Json(req): Json<Value>) -> Response {
    let Some(id) = req.get("id").filter(|v| !v.is_null()).cloned() else {
        return StatusCode::ACCEPTED.into_response(); // notification: nothing to answer
    };
    let result = match req["method"].as_str().unwrap_or_default() {
        "initialize" => Ok(json!({
            "protocolVersion": req["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "vigia", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "Vigía watches web pages for price, stock and text changes. Prices are numbers in the watch's currency; times are unix seconds."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => {
            let out = call(&app, req["params"]["name"].as_str().unwrap_or_default(), &req["params"]["arguments"]).await;
            let (text, is_error) = match out {
                Ok(v) => (v.to_string(), false),
                Err(e) => (e.1, true),
            };
            Ok(json!({ "content": [{ "type": "text", "text": text }], "isError": is_error }))
        }
        m => Err(json!({ "code": -32601, "message": format!("Method not found: {m}") })),
    };
    Json(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
    })
    .into_response()
}
