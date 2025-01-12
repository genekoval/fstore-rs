use crate::{
    db::{self, Database},
    error::{Error, OptionNotFound, Result},
    fs::{Filesystem, Part},
    model::*,
    progress::{Progress, ProgressGuard, Task},
    DbConnection, DbSupport,
};

use chrono::{DateTime, Local};
use fstore::{Bucket, Object, ObjectError, RemoveResult, StoreTotals};
use futures::stream::StreamExt;
use log::{error, info, trace};
use pgtools::{PgDump, PgRestore, Psql};
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    path::{Path, PathBuf},
    result,
    sync::Arc,
};
use tokio::{
    fs::File,
    sync::Semaphore,
    task::{self, JoinHandle},
};
use tokio_util::task::TaskTracker;
use uuid::Uuid;

const DATABASE_DUMP_FILENAME: &str = "fstore.dump";
const DEFAULT_SQL_DIRECTORY: &str =
    match option_env!("FSTORE_DEFAULT_SQL_DIRECTORY") {
        Some(dir) => dir,
        None => "db",
    };

#[derive(Debug, Deserialize, Serialize)]
pub struct DatabaseConfig {
    pub connection: DbConnection,

    #[serde(default = "DatabaseConfig::default_max_connections")]
    pub max_connections: u32,

    #[serde(default)]
    pub psql: Psql,

    #[serde(default)]
    pub pg_dump: PgDump,

    #[serde(default)]
    pub pg_restore: PgRestore,

    #[serde(default = "DatabaseConfig::default_sql_directory")]
    pub sql_directory: PathBuf,
}

impl DatabaseConfig {
    fn default_max_connections() -> u32 {
        10
    }

    fn default_sql_directory() -> PathBuf {
        DEFAULT_SQL_DIRECTORY.into()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct StoreOptions<'a> {
    pub version: Version,
    pub database: &'a DatabaseConfig,
    pub home: &'a Path,
    pub archive: &'a Option<PathBuf>,
}

trait ObjectStreamAction: Clone + Send + Sync + 'static {
    fn run(
        &self,
        store: &ObjectStore,
        object: &db::Object,
    ) -> impl Future<Output = result::Result<(), String>> + Send;
}

#[derive(Clone, Copy, Debug)]
struct CheckAction;

impl ObjectStreamAction for CheckAction {
    async fn run(
        &self,
        store: &ObjectStore,
        object: &db::Object,
    ) -> result::Result<(), String> {
        store
            .filesystem
            .check(&object.object_id, &object.hash)
            .await
    }
}

#[derive(Clone, Debug)]
struct SyncAction {
    archive: Arc<PathBuf>,
}

impl SyncAction {
    fn new(path: &Path) -> Self {
        Self {
            archive: Arc::new(path.to_owned()),
        }
    }
}

impl ObjectStreamAction for SyncAction {
    async fn run(
        &self,
        store: &ObjectStore,
        object: &db::Object,
    ) -> result::Result<(), String> {
        store
            .filesystem
            .copy(&object.object_id, self.archive.as_path(), &object.hash)
            .await
    }
}

#[derive(Debug, Default)]
pub struct Tasks {
    pub archive: Task,
    pub check: Task,
}

pub struct ObjectStore {
    pub tasks: Tasks,

    about: About,
    database: Database,
    db_support: DbSupport,
    filesystem: Filesystem,
    archive: Option<PathBuf>,
}

impl ObjectStore {
    pub async fn new(
        options: StoreOptions<'_>,
    ) -> result::Result<Self, String> {
        let database = Database::from_config(options.database).await?;
        let db_support = DbSupport::new(
            options.version.number,
            pgtools::Options {
                connection: &options.database.connection,
                psql: &options.database.psql,
                pg_dump: &options.database.pg_dump,
                pg_restore: &options.database.pg_restore,
                sql_directory: &options.database.sql_directory,
            },
        )?;

        Ok(Self {
            about: About {
                version: options.version,
            },
            database,
            db_support,
            filesystem: Filesystem::new(options.home),
            archive: options.archive.clone(),
            tasks: Default::default(),
        })
    }

    pub async fn prepare(&self) -> result::Result<(), String> {
        self.db_support.check_schema_version().await?;

        Ok(())
    }

    pub async fn archive(
        self: Arc<Self>,
    ) -> Result<(Progress, JoinHandle<Result<()>>)> {
        let archive = self.archive.as_deref().ok_or_else(|| {
            Error::Internal("archive location not specified".into())
        })?;

        let started = Local::now();
        let total = self.get_object_count(started).await?;
        let guard =
            ProgressGuard::new(started, total, self.tasks.archive.clone())?;

        tokio::fs::create_dir_all(archive).await.map_err(|err| {
            Error::Internal(format!(
                "Failed to create archive directory '{}': {err}",
                archive.display()
            ))
        })?;

        let dump = archive.join(DATABASE_DUMP_FILENAME);
        self.db_support.dump(&dump).await.map_err(Error::Internal)?;

        self.filesystem.remove_extraneous(archive).await?;

        let progress = guard.clone();
        let action = SyncAction::new(archive);

        let handle =
            task::spawn(
                async move { self.for_each_object(guard, action).await },
            );

        Ok((progress, handle))
    }

    pub async fn check(
        self: Arc<Self>,
    ) -> Result<(Progress, JoinHandle<Result<()>>)> {
        let started = Local::now();
        let total = self.get_object_count(started).await?;
        let guard =
            ProgressGuard::new(started, total, self.tasks.check.clone())?;

        let progress = guard.clone();

        let handle = task::spawn(async move {
            self.for_each_object(guard, CheckAction).await
        });

        Ok((progress, handle))
    }

    pub async fn init(&self) -> result::Result<(), String> {
        self.db_support.init().await
    }

    pub async fn migrate(&self) -> result::Result<(), String> {
        self.db_support.migrate().await
    }

    pub async fn reset(&self) -> result::Result<(), String> {
        self.db_support.reset().await
    }

    pub async fn restore(&self, path: &Path) -> result::Result<(), String> {
        self.db_support.restore(path).await
    }

    pub fn about(&self) -> &About {
        &self.about
    }

    pub async fn add_bucket(&self, name: &str) -> Result<Bucket> {
        Ok(self.database.create_bucket(name).await?.into())
    }

    pub async fn clone_bucket(
        &self,
        original: Uuid,
        name: &str,
    ) -> Result<Bucket> {
        Ok(self.database.clone_bucket(original, name).await?.into())
    }

    pub async fn commit_part(
        &self,
        bucket_id: &Uuid,
        part_id: &Uuid,
    ) -> Result<Object> {
        let metadata = self.filesystem.commit(part_id).await?;

        Ok(self
            .database
            .add_object(
                bucket_id,
                &metadata.id,
                metadata.hash.as_str(),
                metadata.size.try_into().unwrap(),
                metadata.r#type.as_str(),
                metadata.subtype.as_str(),
            )
            .await?
            .into())
    }

    pub async fn get_all_objects(
        &self,
        bucket_id: Uuid,
    ) -> Result<Vec<Object>> {
        Ok(self
            .database
            .get_bucket_objects(bucket_id)
            .await?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    pub async fn get_bucket(&self, name: &str) -> Result<Bucket> {
        Ok(self.database.fetch_bucket(name).await?.into())
    }

    pub async fn get_buckets(&self) -> Result<Vec<Bucket>> {
        Ok(self
            .database
            .fetch_buckets_all()
            .await?
            .into_iter()
            .map(|bucket| bucket.into())
            .collect())
    }

    pub async fn get_object(&self, object_id: &Uuid) -> Result<File> {
        self.filesystem.object(object_id).await
    }

    pub async fn get_object_errors(&self) -> Result<Vec<ObjectError>> {
        Ok(self
            .database
            .get_errors()
            .await?
            .into_iter()
            .map(|errors| errors.into())
            .collect())
    }

    pub async fn get_object_metadata(
        &self,
        bucket_id: Uuid,
        object_id: Uuid,
    ) -> Result<Object> {
        self.get_objects(bucket_id, &[object_id])
            .await?
            .pop()
            .ok_or_not_found("Bucket or object not found")
    }

    pub async fn get_objects(
        &self,
        bucket_id: Uuid,
        objects: &[Uuid],
    ) -> Result<Vec<Object>> {
        let objects = self
            .database
            .get_objects(bucket_id, objects)
            .await?
            .into_iter()
            .map(Into::into)
            .collect();

        Ok(objects)
    }

    pub async fn get_part(&self, part_id: Option<&Uuid>) -> Result<Part> {
        let generated;
        let id = match part_id {
            Some(id) => id,
            None => {
                generated = Uuid::new_v4();
                &generated
            }
        };

        self.filesystem.part(id).await
    }

    pub async fn get_totals(&self) -> Result<StoreTotals> {
        Ok(self.database.fetch_store_totals().await?.into())
    }

    pub async fn prune(&self) -> Result<Vec<Object>> {
        let mut tx = self.database.begin().await?;
        let objects = tx.remove_orphan_objects().await?;

        self.filesystem
            .remove_objects(objects.iter().map(|obj| &obj.object_id))
            .await?;

        tx.commit().await?;

        info!(
            "Pruned {} object{}",
            objects.len(),
            match objects.len() {
                1 => "",
                _ => "s",
            }
        );

        Ok(objects.into_iter().map(|obj| obj.into()).collect())
    }

    pub async fn remove_bucket(&self, bucket_id: &Uuid) -> Result<()> {
        Ok(self.database.remove_bucket(bucket_id).await?)
    }

    pub async fn remove_object(
        &self,
        bucket_id: &Uuid,
        object_id: &Uuid,
    ) -> Result<Object> {
        self.database
            .remove_object(bucket_id, object_id)
            .await?
            .map(|object| object.into())
            .ok_or_not_found("Bucket or object not found")
    }

    pub async fn remove_objects(
        &self,
        bucket_id: &Uuid,
        objects: &[Uuid],
    ) -> Result<RemoveResult> {
        Ok(self
            .database
            .remove_objects(bucket_id, objects)
            .await?
            .into())
    }

    pub async fn rename_bucket(
        &self,
        bucket_id: &Uuid,
        new_name: &str,
    ) -> Result<()> {
        Ok(self.database.rename_bucket(bucket_id, new_name).await?)
    }

    pub async fn shutdown(&self) {
        self.database.close().await
    }

    async fn get_object_count(&self, start: DateTime<Local>) -> Result<u64> {
        let total = self
            .database
            .get_object_count(start)
            .await
            .map_err(|err| {
                Error::Internal(format!("failed to fetch object count: {err}"))
            })?
            .try_into()
            .unwrap();

        Ok(total)
    }

    async fn for_each_object(
        self: Arc<Self>,
        progress: ProgressGuard,
        action: impl ObjectStreamAction,
    ) -> Result<()> {
        let tracker = TaskTracker::new();
        let semaphore = Arc::new(Semaphore::new(num_cpus::get()));
        let mut error: Option<Error> = None;
        let mut stream = self.database.stream_objects(progress.started());

        'stream: while let Some(object) = stream.next().await {
            let object = match object {
                Ok(object) => object,
                Err(err) => {
                    error = Some(Error::Internal(format!(
                        "failed to fetch object from database: {err}"
                    )));
                    break 'stream;
                }
            };

            let permit = semaphore.clone().acquire_owned().await.unwrap();
            let store = self.clone();
            let progress = progress.clone();
            let action = action.clone();

            tracker.spawn(async move {
                let messages = match action.run(&store, &object).await {
                    Ok(()) => progress.clear_error(object.object_id),
                    Err(message) => progress.error(object.object_id, message),
                };

                progress.increment();
                drop(permit);

                if !messages.is_empty() {
                    if let Err(err) =
                        store.database.update_object_errors(&messages).await
                    {
                        error!("failed to update object errors: {err}");
                    }
                }

                trace!("Processed object {}", object.object_id);
            });
        }

        tracker.close();
        tracker.wait().await;

        let messages = progress.messages();
        if !messages.is_empty() {
            if let Err(err) =
                self.database.update_object_errors(&messages).await
            {
                error!("failed to update object errors: {err}");
            }
        }

        match error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}
