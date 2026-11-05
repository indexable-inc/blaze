//! A small service: axum in front of SQLite through sqlx. `websvc <port>` serves until it is
//! killed; the harness asks it things with curl and loads it with oha.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sqlx::{sqlite::SqlitePoolOptions, FromRow, SqlitePool};

#[derive(Serialize, FromRow)]
struct Note {
    id: i64,
    author: String,
    body: String,
    stars: i64,
}

#[derive(Deserialize)]
struct NewNote {
    author: String,
    body: String,
}

#[derive(Deserialize)]
struct Page {
    #[serde(default)]
    after: i64,
    #[serde(default = "page_size")]
    limit: i64,
}

fn page_size() -> i64 {
    10
}

#[derive(Serialize)]
struct Problem {
    error: String,
}

type Failure = (StatusCode, Json<Problem>);

fn failure(status: StatusCode, error: impl ToString) -> Failure {
    (status, Json(Problem { error: error.to_string() }))
}

fn internal(error: sqlx::Error) -> Failure {
    failure(StatusCode::INTERNAL_SERVER_ERROR, error)
}

async fn list(State(pool): State<SqlitePool>, Query(page): Query<Page>) -> Result<Json<Vec<Note>>, Failure> {
    if !(1..=100).contains(&page.limit) {
        return Err(failure(StatusCode::BAD_REQUEST, "limit is between 1 and 100"));
    }
    let notes = sqlx::query_as("SELECT id, author, body, stars FROM notes WHERE id > ? ORDER BY id LIMIT ?")
        .bind(page.after)
        .bind(page.limit)
        .fetch_all(&pool)
        .await
        .map_err(internal)?;
    Ok(Json(notes))
}

async fn one(State(pool): State<SqlitePool>, Path(id): Path<i64>) -> Result<Json<Note>, Failure> {
    let note = sqlx::query_as("SELECT id, author, body, stars FROM notes WHERE id = ?")
        .bind(id)
        .fetch_optional(&pool)
        .await
        .map_err(internal)?;
    note.map(Json).ok_or_else(|| failure(StatusCode::NOT_FOUND, format!("no note {id}")))
}

async fn create(State(pool): State<SqlitePool>, Json(new): Json<NewNote>) -> Result<(StatusCode, Json<Note>), Failure> {
    if new.body.is_empty() {
        return Err(failure(StatusCode::UNPROCESSABLE_ENTITY, "a note has a body"));
    }
    let id = sqlx::query("INSERT INTO notes (author, body, stars) VALUES (?, ?, 0)")
        .bind(&new.author)
        .bind(&new.body)
        .execute(&pool)
        .await
        .map_err(internal)?
        .last_insert_rowid();
    Ok((StatusCode::CREATED, Json(Note { id, author: new.author, body: new.body, stars: 0 })))
}

async fn star(State(pool): State<SqlitePool>, Path(id): Path<i64>) -> Result<Json<Note>, Failure> {
    let mut tx = pool.begin().await.map_err(internal)?;
    let changed = sqlx::query("UPDATE notes SET stars = stars + 1 WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    if changed.rows_affected() == 0 {
        return Err(failure(StatusCode::NOT_FOUND, format!("no note {id}")));
    }
    let note = sqlx::query_as("SELECT id, author, body, stars FROM notes WHERE id = ?")
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    Ok(Json(note))
}

async fn stats(State(pool): State<SqlitePool>) -> Result<Json<serde_json::Value>, Failure> {
    let (count, stars, longest): (i64, Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT COUNT(*), SUM(stars), MAX(LENGTH(body)) FROM notes")
            .fetch_one(&pool)
            .await
            .map_err(internal)?;
    let authors: Vec<(String, i64)> =
        sqlx::query_as("SELECT author, COUNT(*) FROM notes GROUP BY author ORDER BY COUNT(*) DESC, author LIMIT 3")
            .fetch_all(&pool)
            .await
            .map_err(internal)?;
    Ok(Json(serde_json::json!({ "notes": count, "stars": stars, "longest": longest, "authors": authors })))
}

async fn seed(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query("CREATE TABLE notes (id INTEGER PRIMARY KEY, author TEXT NOT NULL, body TEXT NOT NULL, stars INTEGER NOT NULL)")
        .execute(pool)
        .await?;
    let mut tx = pool.begin().await?;
    for n in 0..500i64 {
        sqlx::query("INSERT INTO notes (author, body, stars) VALUES (?, ?, ?)")
            .bind(format!("author{}", n % 7))
            .bind(format!("note {n} says {}", "la ".repeat((n % 11) as usize)))
            .bind(n % 5)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port: u16 = std::env::args().nth(1).ok_or("usage: websvc <port>")?.parse()?;
    // One connection: the database is in memory, and each connection would have its own.
    let pool = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await?;
    seed(&pool).await?;
    let app = Router::new()
        .route("/health", get(|| async { "ok\n" }))
        .route("/notes", get(list).post(create))
        .route("/notes/:id", get(one))
        .route("/notes/:id/star", post(star))
        .route("/stats", get(stats))
        .with_state(pool);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
