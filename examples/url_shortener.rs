use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::post;
use axum::{async_trait, extract::FromRequest, response::IntoResponse, routing::get, Json, Router};
use mockall::automock;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use std::sync::Arc;
use thiserror::Error;
use tokio::net::TcpListener;
use tracing::{info, warn};
use tracing_subscriber::{
    filter::LevelFilter, fmt, fmt::format::FmtSpan, layer::SubscriberExt, util::SubscriberInitExt,
    Layer,
};

const LISTEN_ADDR: &str = "127.0.0.1:19091";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let console = fmt::Layer::new()
        .with_span_events(FmtSpan::CLOSE)
        .pretty()
        .with_filter(LevelFilter::DEBUG);

    tracing_subscriber::registry().with(console).init();

    let listener = TcpListener::bind(LISTEN_ADDR).await?;
    info!("start serving on {}", LISTEN_ADDR);

    let pq_schema = "postgresql://postgres:test@127.0.0.1:5432/test";
    let registry = Repository::try_new(pq_schema).await?;
    registry.migrate().await?;

    let service = Arc::new(Service {
        repository: Arc::new(registry),
        id_generator: Arc::new(NanoGenerator),
        cnt: 3,
    });

    let app = Router::new()
        .route("/shorten", post(shorten))
        .route("/redirect/:id", get(redirect))
        .with_state(service);

    axum::serve(listener, app.into_make_service()).await?;

    Ok(())
}

#[automock]
pub trait IdGenerator: Send + Sync {
    fn generate(&self) -> String;
}
struct NanoGenerator;

impl IdGenerator for NanoGenerator {
    fn generate(&self) -> String {
        nanoid::nanoid!(6)
    }
}

struct Service {
    repository: Arc<Repository>,
    id_generator: Arc<dyn IdGenerator>,
    cnt: i32,
}

impl Service {
    async fn shorten(&self, url: &str) -> anyhow::Result<String> {
        for _i in 0..self.cnt {
            let n_id = self.id_generator.generate();
            match self.repository.insert_url(url, &n_id).await {
                Ok(id) => return Ok(format!("http://{}/redirect/{}", LISTEN_ADDR, id)),
                Err(e) => match e {
                    sqlx::Error::Database(e) => match e.code() {
                        Some(code) => {
                            if code == "23505" {
                                warn!("duplicated id: {}", n_id)
                            } else {
                                return Err(anyhow::anyhow!(e));
                            }
                        }
                        None => return Err(anyhow::anyhow!(e)),
                    },
                    _ => return Err(anyhow::anyhow!(e)),
                },
            }
        }
        Err(anyhow::anyhow!("retry limit"))
    }

    async fn redirect(&self, id: &str) -> anyhow::Result<String> {
        // TODO add err parse
        self.repository.find_url(id).await
    }
}

pub trait ShortenRepository {
    fn shorten(&self, url: &str) -> anyhow::Result<String>;
    fn redirect(&self, id: &str) -> anyhow::Result<String>;
}

struct Repository {
    pool: PgPool,
}

#[derive(Error, Debug)]
pub enum ShortenError {
    #[error("invalid JSON: {0}")]
    InValidJsonInput(JsonRejection),
    #[error("unknown data store error")]
    Unknown,
    #[error("internal error")]
    InternalError,
}

#[derive(Debug, FromRow)]
struct ShortenerDao {
    #[sqlx(default)]
    id: String,
    #[sqlx(default)]
    url: String,
}

impl Repository {
    async fn try_new(schema: &str) -> anyhow::Result<Self> {
        let pool = PgPool::connect(schema).await?;
        Ok(Self { pool })
    }

    async fn migrate(&self) -> anyhow::Result<()> {
        sqlx::query(
            r#"
        CREATE TABLE IF NOT EXISTS shorteners (
            id CHAR(6) PRIMARY KEY,
            url TEXT NOT NULL UNIQUE
        )
            "#,
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn insert_url(&self, url: &str, id: &str) -> anyhow::Result<String, sqlx::Error> {
        let row :ShortenerDao = sqlx::query_as("INSERT INTO shorteners (id, url) VALUES ($1, $2) ON CONFLICT(url) DO UPDATE SET url=EXCLUDED.url RETURNING id")
            .bind(id)
            .bind(url)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.id)
    }

    async fn find_url(&self, id: &str) -> anyhow::Result<String> {
        let row: ShortenerDao = sqlx::query_as("SELECT (url) FROM shorteners WHERE id = $1")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.url)
    }

    #[allow(dead_code)]
    async fn delete_by_id(&self, id: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM shorteners WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct ShortenReq {
    url: String,
}

#[derive(Debug, Serialize)]
struct ShortenRes {
    url: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct JsonParser<T>(pub T);

#[async_trait]
impl<T, S> FromRequest<S> for JsonParser<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ShortenError;

    async fn from_request(req: Request, state: &S) -> std::result::Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(json) => Ok(JsonParser(json.0)),
            Err(e) => Err(ShortenError::InValidJsonInput(e)),
        }
    }
}

impl IntoResponse for ShortenError {
    fn into_response(self) -> Response {
        #[derive(Debug, Serialize)]
        struct JsonErr {
            code: i32,
            msg: String,
        }
        // distinguish external or internal msg
        // code explain:
        //      x      -  xx      -  xxx
        //      {type} - {module} -  {number}
        //      5         10         001
        match self {
            ShortenError::Unknown => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(JsonErr {
                    code: 510001,
                    msg: "system unknown err".to_string(),
                }),
            )
                .into_response(),
            ShortenError::InternalError => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(JsonErr {
                    code: 510002,
                    msg: "system internal err".to_string(),
                }),
            )
                .into_response(),
            ShortenError::InValidJsonInput { .. } => (
                StatusCode::BAD_REQUEST,
                Json(JsonErr {
                    code: 410012,
                    msg: "Invalid input".to_string(),
                }),
            )
                .into_response(),
        }
    }
}

async fn shorten(
    State(service): State<Arc<Service>>,
    JsonParser(url): JsonParser<ShortenReq>,
) -> Result<impl IntoResponse, StatusCode> {
    info!("url needed to be shortened: {}", url.url);
    let id = service.shorten(&url.url).await.map_err(|e| {
        warn!("shorten failed: {}", e);
        StatusCode::NOT_FOUND
    })?;
    Ok((
        StatusCode::CREATED,
        Json(ShortenRes {
            url: format!("http://{}/redirect/{}", LISTEN_ADDR, id),
        }),
    ))
}

async fn redirect(
    State(service): State<Arc<Service>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ShortenError> {
    info!("redirecting to id: {}", id);
    let url = service.redirect(&id).await.map_err(|e| {
        warn!("redirect failed: {}", e);
        ShortenError::InternalError
    })?;
    let mut headers = HeaderMap::new();
    headers.insert("Location", url.parse().unwrap());
    Ok((StatusCode::PERMANENT_REDIRECT, headers))
}

#[cfg(test)]
mod test {
    use crate::*;
    use mockall::Sequence;
    use nanoid::nanoid;

    #[tokio::test]
    async fn test_shorten_duplicated() -> anyhow::Result<()> {
        let pq_schema = "postgresql://postgres:test@127.0.0.1:5432/test";
        let repository = Repository::try_new(pq_schema).await?;
        repository.migrate().await?;
        let nano_id = nanoid!(6);
        repository.insert_url("http://127.0.0.1", &nano_id).await?;

        let mut generator = MockIdGenerator::new();
        let mut seq = Sequence::new();
        let id1_clone = nano_id.clone();
        generator
            .expect_generate()
            .times(1)
            .in_sequence(&mut seq)
            .returning(move || id1_clone.clone());
        let nano_id2 = nanoid!(6);
        let id2_clone = nano_id2.clone();
        generator
            .expect_generate()
            .times(1)
            .in_sequence(&mut seq)
            .returning(move || id2_clone.clone());

        let service = Service {
            repository: Arc::new(repository),
            cnt: 3,
            id_generator: Arc::new(generator),
        };

        let url = service.shorten("http://abc.de.ef.g").await.unwrap();
        service.repository.delete_by_id(&nano_id).await?;
        service.repository.delete_by_id(&nano_id2).await?;
        assert_eq!(url, format!("http://{}/redirect/{}", LISTEN_ADDR, nano_id2));

        Ok(())
    }
}
