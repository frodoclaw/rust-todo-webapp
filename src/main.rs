use std::{
    ffi::{c_char, c_int, c_uchar, c_void, CStr, CString},
    net::SocketAddr,
    path::Path as FsPath,
    ptr,
    sync::{Arc, Mutex, OnceLock},
};

use axum::{
    extract::{Path, State},
    http::{header, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, patch},
    Json, Router,
};
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

    axum::serve(listener, app)
        .await
        .expect("server error");
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
    db.execute(
        r#"
        CREATE TABLE IF NOT EXISTS todos (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            title TEXT NOT NULL,
            completed INTEGER NOT NULL DEFAULT 0
        )
        "#,
    )?;
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
    handle: *mut c_void,
}

unsafe impl Send for Db {}

impl Db {
    fn open(path: &FsPath) -> Result<Self, DbError> {
        let api = sqlite_api()?;
        let path = path
            .to_str()
            .ok_or_else(|| DbError::new("database path contains invalid UTF-8"))?;
        let path = CString::new(path).map_err(|_| DbError::new("database path contains NUL"))?;
        let mut handle = ptr::null_mut();

        let rc = unsafe {
            (api.open_v2)(
                path.as_ptr(),
                &mut handle,
                SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_FULLMUTEX,
                ptr::null(),
            )
        };
        if rc != SQLITE_OK {
            let message = sqlite_error_message(api, handle, rc);
            if !handle.is_null() {
                unsafe { (api.close_v2)(handle) };
            }
            return Err(DbError::new(message));
        }

        let mut db = Self { handle };
        db.set_busy_timeout(5_000)?;
        db.execute("PRAGMA journal_mode = WAL")?;
        db.execute("PRAGMA synchronous = NORMAL")?;
        Ok(db)
    }

    fn set_busy_timeout(&self, millis: c_int) -> Result<(), DbError> {
        let api = sqlite_api()?;
        let rc = unsafe { (api.busy_timeout)(self.handle, millis) };
        if rc == SQLITE_OK {
            Ok(())
        } else {
            Err(DbError::new(sqlite_error_message(api, self.handle, rc)))
        }
    }

    fn execute(&mut self, sql: &str) -> Result<(), DbError> {
        let mut statement = Statement::prepare(self.handle, sql)?;
        loop {
            match statement.step()? {
                StepResult::Done => return Ok(()),
                StepResult::Row => {}
            }
        }
    }

    fn list_todos(&self) -> Result<Vec<Todo>, DbError> {
        let mut statement = Statement::prepare(
            self.handle,
            "SELECT id, title, completed FROM todos ORDER BY id ASC",
        )?;
        let mut todos = Vec::new();

        while let StepResult::Row = statement.step()? {
            todos.push(Todo {
                id: statement.column_i64(0),
                title: statement.column_text(1)?,
                completed: statement.column_i64(2) != 0,
            });
        }

        Ok(todos)
    }

    fn create_todo(&self, title: &str) -> Result<Todo, DbError> {
        let mut statement =
            Statement::prepare(self.handle, "INSERT INTO todos (title, completed) VALUES (?, ?)")?;
        statement.bind_text(1, title)?;
        statement.bind_i64(2, 0)?;

        match statement.step()? {
            StepResult::Done => Ok(Todo {
                id: self.last_insert_rowid()?,
                title: title.to_string(),
                completed: false,
            }),
            StepResult::Row => Err(DbError::new("unexpected row returned")),
        }
    }

    fn toggle_todo(&self, id: i64) -> Result<Option<Todo>, DbError> {
        let todo = {
            let mut statement = Statement::prepare(
                self.handle,
                "SELECT id, title, completed FROM todos WHERE id = ?",
            )?;
            statement.bind_i64(1, id)?;

            match statement.step()? {
                StepResult::Row => Some(Todo {
                    id: statement.column_i64(0),
                    title: statement.column_text(1)?,
                    completed: statement.column_i64(2) != 0,
                }),
                StepResult::Done => None,
            }
        };

        let Some(mut todo) = todo else {
            return Ok(None);
        };

        todo.completed = !todo.completed;
        let mut statement =
            Statement::prepare(self.handle, "UPDATE todos SET completed = ? WHERE id = ?")?;
        statement.bind_i64(1, if todo.completed { 1 } else { 0 })?;
        statement.bind_i64(2, todo.id)?;

        match statement.step()? {
            StepResult::Done => Ok(Some(todo)),
            StepResult::Row => Err(DbError::new("unexpected row returned")),
        }
    }

    fn delete_todo(&self, id: i64) -> Result<bool, DbError> {
        let mut statement = Statement::prepare(self.handle, "DELETE FROM todos WHERE id = ?")?;
        statement.bind_i64(1, id)?;

        match statement.step()? {
            StepResult::Done => Ok(self.changes()? > 0),
            StepResult::Row => Err(DbError::new("unexpected row returned")),
        }
    }

    fn last_insert_rowid(&self) -> Result<i64, DbError> {
        let api = sqlite_api()?;
        Ok(unsafe { (api.last_insert_rowid)(self.handle) })
    }

    fn changes(&self) -> Result<c_int, DbError> {
        let api = sqlite_api()?;
        Ok(unsafe { (api.changes)(self.handle) })
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        if let Ok(api) = sqlite_api() {
            unsafe { (api.close_v2)(self.handle) };
        }
    }
}

struct Statement {
    db: *mut c_void,
    handle: *mut c_void,
}

impl Statement {
    fn prepare(db: *mut c_void, sql: &str) -> Result<Self, DbError> {
        let api = sqlite_api()?;
        let sql = CString::new(sql).map_err(|_| DbError::new("SQL contains NUL"))?;
        let mut handle = ptr::null_mut();
        let rc = unsafe { (api.prepare_v2)(db, sql.as_ptr(), -1, &mut handle, ptr::null_mut()) };
        if rc != SQLITE_OK {
            return Err(DbError::new(sqlite_error_message(api, db, rc)));
        }

        Ok(Self { db, handle })
    }

    fn bind_i64(&mut self, index: c_int, value: i64) -> Result<(), DbError> {
        let api = sqlite_api()?;
        let rc = unsafe { (api.bind_int64)(self.handle, index, value) };
        if rc == SQLITE_OK {
            Ok(())
        } else {
            Err(DbError::new(sqlite_error_message(api, self.db, rc)))
        }
    }

    fn bind_text(&mut self, index: c_int, value: &str) -> Result<(), DbError> {
        let api = sqlite_api()?;
        let value = CString::new(value).map_err(|_| DbError::new("text contains NUL"))?;
        let rc = unsafe {
            (api.bind_text)(
                self.handle,
                index,
                value.as_ptr(),
                -1,
                sqlite_transient_destructor(),
            )
        };
        if rc == SQLITE_OK {
            Ok(())
        } else {
            Err(DbError::new(sqlite_error_message(api, self.db, rc)))
        }
    }

    fn step(&mut self) -> Result<StepResult, DbError> {
        let api = sqlite_api()?;
        match unsafe { (api.step)(self.handle) } {
            SQLITE_ROW => Ok(StepResult::Row),
            SQLITE_DONE => Ok(StepResult::Done),
            rc => Err(DbError::new(sqlite_error_message(api, self.db, rc))),
        }
    }

    fn column_i64(&self, index: c_int) -> i64 {
        let api = sqlite_api().expect("sqlite API should be loaded");
        unsafe { (api.column_int64)(self.handle, index) }
    }

    fn column_text(&self, index: c_int) -> Result<String, DbError> {
        let api = sqlite_api()?;
        let ptr = unsafe { (api.column_text)(self.handle, index) };
        if ptr.is_null() {
            return Ok(String::new());
        }

        let text = unsafe { CStr::from_ptr(ptr.cast::<c_char>()) };
        Ok(text.to_string_lossy().into_owned())
    }
}

impl Drop for Statement {
    fn drop(&mut self) {
        if let Ok(api) = sqlite_api() {
            unsafe { (api.finalize)(self.handle) };
        }
    }
}

enum StepResult {
    Row,
    Done,
}

type Sqlite3Destructor = unsafe extern "C" fn(*mut c_void);

struct SqliteApi {
    _lib: *mut c_void,
    open_v2: unsafe extern "C" fn(*const c_char, *mut *mut c_void, c_int, *const c_char) -> c_int,
    close_v2: unsafe extern "C" fn(*mut c_void) -> c_int,
    busy_timeout: unsafe extern "C" fn(*mut c_void, c_int) -> c_int,
    errmsg: unsafe extern "C" fn(*mut c_void) -> *const c_char,
    prepare_v2:
        unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *mut *mut c_void, *mut *const c_char) -> c_int,
    bind_int64: unsafe extern "C" fn(*mut c_void, c_int, i64) -> c_int,
    bind_text:
        unsafe extern "C" fn(*mut c_void, c_int, *const c_char, c_int, Sqlite3Destructor) -> c_int,
    step: unsafe extern "C" fn(*mut c_void) -> c_int,
    finalize: unsafe extern "C" fn(*mut c_void) -> c_int,
    column_int64: unsafe extern "C" fn(*mut c_void, c_int) -> i64,
    column_text: unsafe extern "C" fn(*mut c_void, c_int) -> *const c_uchar,
    last_insert_rowid: unsafe extern "C" fn(*mut c_void) -> i64,
    changes: unsafe extern "C" fn(*mut c_void) -> c_int,
}

unsafe impl Send for SqliteApi {}
unsafe impl Sync for SqliteApi {}

fn sqlite_api() -> Result<&'static SqliteApi, DbError> {
    static API: OnceLock<Result<SqliteApi, DbError>> = OnceLock::new();
    API.get_or_init(SqliteApi::load).as_ref().map_err(Clone::clone)
}

impl SqliteApi {
    fn load() -> Result<Self, DbError> {
        let path = CString::new("/usr/lib/x86_64-linux-gnu/libsqlite3.so.0")
            .map_err(|_| DbError::new("invalid SQLite library path"))?;
        let handle = unsafe { dlopen(path.as_ptr(), RTLD_NOW | RTLD_LOCAL) };
        if handle.is_null() {
            return Err(DbError::new(dl_last_error()));
        }

        unsafe {
            Ok(Self {
                _lib: handle,
                open_v2: load_symbol(handle, "sqlite3_open_v2")?,
                close_v2: load_symbol(handle, "sqlite3_close_v2")?,
                busy_timeout: load_symbol(handle, "sqlite3_busy_timeout")?,
                errmsg: load_symbol(handle, "sqlite3_errmsg")?,
                prepare_v2: load_symbol(handle, "sqlite3_prepare_v2")?,
                bind_int64: load_symbol(handle, "sqlite3_bind_int64")?,
                bind_text: load_symbol(handle, "sqlite3_bind_text")?,
                step: load_symbol(handle, "sqlite3_step")?,
                finalize: load_symbol(handle, "sqlite3_finalize")?,
                column_int64: load_symbol(handle, "sqlite3_column_int64")?,
                column_text: load_symbol(handle, "sqlite3_column_text")?,
                last_insert_rowid: load_symbol(handle, "sqlite3_last_insert_rowid")?,
                changes: load_symbol(handle, "sqlite3_changes")?,
            })
        }
    }
}

unsafe fn load_symbol<T>(handle: *mut c_void, symbol: &str) -> Result<T, DbError> {
    let symbol = CString::new(symbol).map_err(|_| DbError::new("invalid SQLite symbol"))?;
    let ptr = dlsym(handle, symbol.as_ptr());
    if ptr.is_null() {
        return Err(DbError::new(dl_last_error()));
    }

    Ok(std::mem::transmute_copy(&ptr))
}

fn sqlite_error_message(api: &SqliteApi, db: *mut c_void, rc: c_int) -> String {
    if db.is_null() {
        return format!("sqlite error code {rc}");
    }

    let message = unsafe { (api.errmsg)(db) };
    if message.is_null() {
        format!("sqlite error code {rc}")
    } else {
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    }
}

fn sqlite_transient_destructor() -> Sqlite3Destructor {
    unsafe { std::mem::transmute::<isize, Sqlite3Destructor>(-1) }
}

#[derive(Clone, Debug)]
struct DbError {
    message: String,
}

impl DbError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DbError {}

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_OPEN_READWRITE: c_int = 0x0000_0002;
const SQLITE_OPEN_CREATE: c_int = 0x0000_0004;
const SQLITE_OPEN_FULLMUTEX: c_int = 0x0001_0000;
const RTLD_NOW: c_int = 2;
const RTLD_LOCAL: c_int = 0;

unsafe extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlerror() -> *const c_char;
}

fn dl_last_error() -> String {
    let message = unsafe { dlerror() };
    if message.is_null() {
        "dynamic loader error".to_string()
    } else {
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{Db, FsPath};
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn sqlite_crud_round_trip() {
        let db_path = temp_db_path();
        let mut db = Db::open(&db_path).expect("open db");
        db.execute(
            r#"
            CREATE TABLE IF NOT EXISTS todos (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                title TEXT NOT NULL,
                completed INTEGER NOT NULL DEFAULT 0
            )
            "#,
        )
        .expect("create table");

        let created = db.create_todo("Write test").expect("create todo");
        assert_eq!(created.title, "Write test");
        assert!(!created.completed);

        let todos = db.list_todos().expect("list todos");
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].id, created.id);

        let toggled = db
            .toggle_todo(created.id)
            .expect("toggle todo")
            .expect("todo exists");
        assert!(toggled.completed);

        let deleted = db.delete_todo(created.id).expect("delete todo");
        assert!(deleted);
        assert!(db.list_todos().expect("list after delete").is_empty());

        drop(db);
        let _ = fs::remove_file(&db_path);
    }

    fn temp_db_path() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        FsPath::new("/tmp").join(format!("rust-todo-webapp-test-{nanos}.db"))
    }
}
