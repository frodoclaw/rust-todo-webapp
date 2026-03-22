use std::{
    net::SocketAddr,
    path::Path as FsPath,
    sync::{Arc, Mutex},
};

use axum::{
    extract::{Path, State},
    http::{header, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, patch},
    Json, Router,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tower_http::{services::ServeDir, trace::TraceLayer};

#[derive(Clone)]
struct AppState {
    db: Arc<Mutex<Db>>,
}

#[derive(Clone, Debug, Serialize)]
struct Todo {
    id: i64,
    title: String,
    completed: bool,
}

#[derive(Debug, Deserialize)]
struct CreateTodo {
    title: String,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

#[tokio::main]
async fn main() {
    let db = init_db().expect("failed to initialize SQLite database");
    let state = AppState {
        db: Arc::new(Mutex::new(db)),
    };

    let api = Router::new()
        .route("/todos", get(list_todos).post(create_todo))
        .route("/todos/:id/toggle", patch(toggle_todo))
        .route("/todos/:id", delete(delete_todo));

    let app = Router::new()
        .nest("/api", api)
        .route("/", get(index))
        .fallback_service(ServeDir::new("static"))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let port = std::env::var("PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(3000);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    println!("listening on http://{addr}");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind TCP listener");

    axum::serve(listener, app).await.expect("server error");
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn list_todos(State(state): State<AppState>) -> Result<Json<Vec<Todo>>, ApiError> {
    let todos = state
        .db
        .lock()
        .map_err(|_| ApiError::internal_message("database lock poisoned"))?
        .list_todos()
        .map_err(ApiError::internal)?;

    Ok(Json(todos))
}

async fn create_todo(
    State(state): State<AppState>,
    Json(payload): Json<CreateTodo>,
) -> Result<(StatusCode, Json<Todo>), ApiError> {
    let title = payload.title.trim();
    if title.is_empty() {
        return Err(ApiError::bad_request("title must not be empty"));
    }

    let todo = state
        .db
        .lock()
        .map_err(|_| ApiError::internal_message("database lock poisoned"))?
        .create_todo(title)
        .map_err(ApiError::internal)?;

    Ok((StatusCode::CREATED, Json(todo)))
}

async fn toggle_todo(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Todo>, ApiError> {
    let todo = state
        .db
        .lock()
        .map_err(|_| ApiError::internal_message("database lock poisoned"))?
        .toggle_todo(id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("todo not found"))?;

    Ok(Json(todo))
}

async fn delete_todo(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let deleted = state
        .db
        .lock()
        .map_err(|_| ApiError::internal_message("database lock poisoned"))?
        .delete_todo(id)
        .map_err(ApiError::internal)?;

    if !deleted {
        return Err(ApiError::not_found("todo not found"));
    }

    Ok(StatusCode::NO_CONTENT)
}

fn init_db() -> Result<Db, DbError> {
    let mut db = Db::open(FsPath::new("todo.db"))?;
    db.init_schema()?;
    Ok(db)
}

struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    fn internal(error: DbError) -> Self {
        eprintln!("database error: {error}");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "internal server error".to_string(),
        }
    }

    fn internal_message(message: impl Into<String>) -> Self {
        eprintln!("database error: {}", message.into());
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "internal server error".to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(ErrorResponse {
            error: self.message,
        });
        let mut response = (self.status, body).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        response
    }
}

struct Db {
    conn: Connection,
}

impl Db {
    fn open(path: &FsPath) -> Result<Self, DbError> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(Self { conn })
    }

    fn init_schema(&mut self) -> Result<(), DbError> {
        self.conn.execute(
            r#"
            CREATE TABLE IF NOT EXISTS todos (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                title TEXT NOT NULL,
                completed INTEGER NOT NULL DEFAULT 0
            )
            "#,
            [],
        )?;
        Ok(())
    }

    fn list_todos(&self) -> Result<Vec<Todo>, DbError> {
        let mut statement = self
            .conn
            .prepare("SELECT id, title, completed FROM todos ORDER BY id ASC")?;
        let rows = statement.query_map([], |row| {
            Ok(Todo {
                id: row.get(0)?,
                title: row.get(1)?,
                completed: row.get::<_, i64>(2)? != 0,
            })
        })?;

        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    fn create_todo(&self, title: &str) -> Result<Todo, DbError> {
        self.conn.execute(
            "INSERT INTO todos (title, completed) VALUES (?, ?)",
            params![title, 0_i64],
        )?;

        Ok(Todo {
            id: self.conn.last_insert_rowid(),
            title: title.to_string(),
            completed: false,
        })
    }

    fn toggle_todo(&self, id: i64) -> Result<Option<Todo>, DbError> {
        let mut todo = self
            .conn
            .query_row(
                "SELECT id, title, completed FROM todos WHERE id = ?",
                params![id],
                |row| {
                    Ok(Todo {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        completed: row.get::<_, i64>(2)? != 0,
                    })
                },
            )
            .optional()?;

        let Some(todo_ref) = todo.as_mut() else {
            return Ok(None);
        };

        todo_ref.completed = !todo_ref.completed;
        self.conn.execute(
            "UPDATE todos SET completed = ? WHERE id = ?",
            params![if todo_ref.completed { 1_i64 } else { 0_i64 }, todo_ref.id],
        )?;

        Ok(todo)
    }

    fn delete_todo(&self, id: i64) -> Result<bool, DbError> {
        let changed = self
            .conn
            .execute("DELETE FROM todos WHERE id = ?", params![id])?;
        Ok(changed > 0)
    }
}

#[derive(Debug)]
struct DbError {
    message: String,
}

impl From<rusqlite::Error> for DbError {
    fn from(error: rusqlite::Error) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DbError {}

#[cfg(test)]
mod tests {
    use super::{Db, DbError, FsPath};
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn sqlite_crud_round_trip() -> Result<(), DbError> {
        let db_path = temp_db_path();
        let result = run_crud_round_trip(&db_path);
        let _ = fs::remove_file(&db_path);
        result
    }

    fn run_crud_round_trip(db_path: &FsPath) -> Result<(), DbError> {
        let mut db = Db::open(db_path)?;
        db.init_schema()?;

        let created = db.create_todo("Write test")?;
        assert_eq!(created.title, "Write test");
        assert!(!created.completed);

        let todos = db.list_todos()?;
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].id, created.id);

        let toggled = db.toggle_todo(created.id)?.expect("todo exists");
        assert!(toggled.completed);

        let deleted = db.delete_todo(created.id)?;
        assert!(deleted);
        assert!(db.list_todos()?.is_empty());

        Ok(())
    }

    fn temp_db_path() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        std::env::temp_dir().join(format!("rust-todo-webapp-test-{nanos}.db"))
    }
}
