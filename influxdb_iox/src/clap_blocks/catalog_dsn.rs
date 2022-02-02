use std::sync::Arc;

use iox_catalog::{
    create_or_get_default_records,
    interface::{Catalog, Error},
    mem::MemCatalog,
    postgres::PostgresCatalog,
};

/// CLI config for catalog DSN.
#[derive(Debug, Clone, clap::Parser)]
pub struct CatalogDsnConfig {
    /// Postgres connection string
    #[clap(long = "--catalog-dsn", env = "INFLUXDB_IOX_CATALOG_DSN")]
    pub dsn: String,
}

impl CatalogDsnConfig {
    pub async fn get_catalog(&self, app_name: &'static str) -> Result<Arc<dyn Catalog>, Error> {
        // If the connection string value is "mem", use an in-memory catalog. Intended for
        // internal testing.
        let catalog = if self.dsn == "mem" {
            let mem = MemCatalog::new();
            create_or_get_default_records(2, &mem).await.unwrap();
            Arc::new(mem) as Arc<dyn Catalog>
        } else {
            Arc::new(
                PostgresCatalog::connect(app_name, iox_catalog::postgres::SCHEMA_NAME, &self.dsn)
                    .await?,
            ) as Arc<dyn Catalog>
        };

        Ok(catalog)
    }
}
