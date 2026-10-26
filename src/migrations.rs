//! Apply unmodified Prisma-generated SQL through SQLx, including in the release binary.
use anyhow::{Context, ensure};
use rust_embed::RustEmbed;
use sqlx::{
    PgPool,
    migrate::{Migration, MigrationType, Migrator},
};
use std::borrow::Cow;

#[derive(RustEmbed)]
#[folder = "db/migrations/"]
struct MigrationFiles;

pub fn migrator() -> anyhow::Result<Migrator> {
    let mut migrations = Vec::new();
    for path in MigrationFiles::iter() {
        if !path.ends_with("/migration.sql") {
            continue;
        }
        let folder = path
            .split('/')
            .next()
            .context("Missing migration directory")?;
        let (version, description) = folder
            .split_once('_')
            .context("Invalid generated migration name")?;
        let version = version
            .parse::<i64>()
            .context("Invalid migration timestamp")?;
        let bytes = MigrationFiles::get(path.as_ref()).context("Missing migration SQL")?;
        let sql =
            String::from_utf8(bytes.data.into_owned()).context("Migration SQL must be UTF-8")?;
        migrations.push(Migration::new(
            version,
            Cow::Owned(description.to_owned()),
            MigrationType::Simple,
            Cow::Owned(sql),
            false,
        ));
    }
    ensure!(
        !migrations.is_empty(),
        "No generated migrations are available"
    );
    migrations.sort_by_key(|migration| migration.version);
    ensure!(
        migrations
            .windows(2)
            .all(|pair| pair[0].version != pair[1].version),
        "Duplicate migration version"
    );
    Ok(Migrator {
        migrations: Cow::Owned(migrations),
        ..Migrator::DEFAULT
    })
}

pub async fn apply(pool: &PgPool) -> anyhow::Result<()> {
    migrator()?
        .run(pool)
        .await
        .context("Could not apply database migrations")
}
