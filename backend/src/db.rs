use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::path::Path;

/// Create the SQLite connection pool and run migrations.
pub async fn init_db() -> SqlitePool {
    dotenvy::dotenv().ok();
    let database_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:voxium.db".into());
    let max_connections = std::env::var("DB_MAX_CONNECTIONS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(16);

    // Create the DB file if it doesn't exist
    let db_path = database_url.trim_start_matches("sqlite:");
    if !Path::new(db_path).exists() {
        std::fs::File::create(db_path).expect("Failed to create database file");
    }

    let pool = SqlitePoolOptions::new()
        .max_connections(max_connections)
        .connect(&database_url)
        .await
        .expect("Failed to connect to SQLite");

    let _ = sqlx::query("PRAGMA journal_mode=WAL")
        .execute(&pool)
        .await;
    let _ = sqlx::query("PRAGMA synchronous=NORMAL")
        .execute(&pool)
        .await;
    let _ = sqlx::query("PRAGMA temp_store=MEMORY")
        .execute(&pool)
        .await;
    let _ = sqlx::query("PRAGMA busy_timeout=5000")
        .execute(&pool)
        .await;
    let _ = sqlx::query("PRAGMA cache_size=-20000")
        .execute(&pool)
        .await;

    let migrations = [
        include_str!("../../migrations/001_init.sql"),
        include_str!("../../migrations/002_add_settings.sql"),
        include_str!("../../migrations/003_add_images.sql"),
        include_str!("../../migrations/004_add_avatar_url.sql"),
        include_str!("../../migrations/005_add_room_kind.sql"),
        include_str!("../../migrations/006_add_banner_url.sql"),
        include_str!("../../migrations/007_add_room_required_role.sql"),
        include_str!("../../migrations/008_add_message_reply.sql"),
        include_str!("../../migrations/009_add_message_pins.sql"),
        include_str!("../../migrations/010_add_server_roles.sql"),
        include_str!("../../migrations/011_add_message_reactions.sql"),
        include_str!("../../migrations/012_add_perf_indexes.sql"),
        include_str!("../../migrations/013_add_discord_oauth.sql"),
    ];

    for sql in migrations {
        run_migration_sql(sql, &pool).await;
    }

    println!("✅ Database initialized");
    pool
}

async fn run_migration_sql(sql_content: &str, pool: &SqlitePool) {
        for statement in sql_content.split(';') {
                let trimmed = statement.trim();
                if !trimmed.is_empty() {
                        sqlx::query(trimmed).execute(pool).await.ok();
                }
        }
}
