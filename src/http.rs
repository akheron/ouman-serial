use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::message;
use crate::server::{Kind, Outcome, Server, Status, SubmitError};

pub fn router(server: Arc<Server>) -> Router {
    Router::new()
        .route("/v1/send", post(send))
        .route("/v1/get", post(get_keyword))
        .route("/v1/set", post(set))
        .route("/v1/select", post(select))
        .route("/v1/status", get(status))
        .layer(middleware::from_fn_with_state(server.clone(), authorize))
        .with_state(server)
}

pub enum ApiError {
    BadRequest(String),
    Unauthorized,
    QueueFull,
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            ApiError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "a valid bearer token is necessary".into(),
            ),
            ApiError::QueueFull => (StatusCode::SERVICE_UNAVAILABLE, "the queue is full".into()),
            ApiError::Internal(message) => (StatusCode::INTERNAL_SERVER_ERROR, message),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

async fn authorize(State(server): State<Arc<Server>>, request: Request, next: Next) -> Response {
    if let Some(token) = &server.config().token {
        let expected = format!("Bearer {token}");
        let given = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        if given != Some(expected.as_str()) {
            return ApiError::Unauthorized.into_response();
        }
    }
    next.run(request).await
}

async fn run(
    server: &Server,
    text: String,
    kind: Kind,
    wait_reply: bool,
) -> Result<Json<Outcome>, ApiError> {
    let request = crate::server::Request {
        text,
        kind,
        wait_reply,
    };
    let outcome = server.submit(request).map_err(|error| match error {
        SubmitError::Invalid(message) => ApiError::BadRequest(message),
        SubmitError::QueueFull => ApiError::QueueFull,
    })?;
    let outcome = outcome
        .await
        .map_err(|_| ApiError::Internal("the dispatcher stopped".into()))?;
    Ok(Json(outcome))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendBody {
    text: String,
    #[serde(default)]
    no_wait: bool,
}

async fn send(
    State(server): State<Arc<Server>>,
    Json(body): Json<SendBody>,
) -> Result<Json<Outcome>, ApiError> {
    if body.text.trim().is_empty() {
        return Err(ApiError::BadRequest("the text is empty".into()));
    }
    run(&server, body.text, Kind::Send, !body.no_wait).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetBody {
    keyword: String,
}

async fn get_keyword(
    State(server): State<Arc<Server>>,
    Json(body): Json<GetBody>,
) -> Result<Json<Outcome>, ApiError> {
    if body.keyword.trim().is_empty() {
        return Err(ApiError::BadRequest("the keyword is empty".into()));
    }
    run(&server, body.keyword.trim().to_string(), Kind::Get, true).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetBody {
    keyword: String,
    fields: Map<String, Value>,
    #[serde(default)]
    no_wait: bool,
}

async fn set(
    State(server): State<Arc<Server>>,
    Json(body): Json<SetBody>,
) -> Result<Json<Outcome>, ApiError> {
    let fields = body
        .fields
        .into_iter()
        .map(|(label, value)| match value {
            Value::String(value) => Ok((label, value)),
            Value::Number(value) => Ok((label, value.to_string())),
            _ => Err(ApiError::BadRequest(format!(
                "the value of {label:?} must be a string or a number"
            ))),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let text = message::build_set(&body.keyword, &fields).map_err(ApiError::BadRequest)?;
    run(&server, text, Kind::Set(fields), !body.no_wait).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectBody {
    keyword: String,
    option: String,
    #[serde(default)]
    no_wait: bool,
}

async fn select(
    State(server): State<Arc<Server>>,
    Json(body): Json<SelectBody>,
) -> Result<Json<Outcome>, ApiError> {
    let text = message::build_select(&body.keyword, &body.option).map_err(ApiError::BadRequest)?;
    run(&server, text, Kind::Select(body.option), !body.no_wait).await
}

async fn status(State(server): State<Arc<Server>>) -> Json<Status> {
    Json(server.status())
}
