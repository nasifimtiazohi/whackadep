//! This module abstracts the communication with crates.io for a given crate
//! Returns Error if the crate is not hosted on crates_io

// TODO: A cheaper way to interact with crates.io can be working with their
// experimental database dump that is updated daily, https://crates.io/data-access,
// which will enable us to avoid making http requests and dealing with rate limits

use anyhow::Result;
use crates_io_api::SyncClient;
use guppy::graph::PackageMetadata;
use tabled::Tabled;

#[derive(Tabled, Default)]
pub struct CratesioReport {
    pub name: String,
    pub is_hosted: bool,
    pub downloads: u64,
    pub dependents: u64,
}

pub struct CratesioAnalyzer {
    client: SyncClient,
}

impl CratesioAnalyzer {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: SyncClient::new(
                "User-Agent: Whackadep (https://github.com/diem/whackadep)",
                std::time::Duration::from_millis(1000),
            )?,
        })
    }

    pub fn analyze_cratesio(self, package: &PackageMetadata) -> Result<CratesioReport> {
        let name = package.name();
        let is_hosted = package.source().is_crates_io();
        self.get_cratesio_metrics(name, is_hosted)
    }

    pub fn get_cratesio_metrics(self, name: &str, is_hosted: bool) -> Result<CratesioReport> {
        if !is_hosted {
            return Ok(CratesioReport {
                name: name.to_string(),
                is_hosted,
                ..Default::default()
            });
        }

        let crate_info = self.client.get_crate(name)?.crate_data;

        // TODO: crates_io_api makes unnecessary request to page through all dependents,
        // therefore, taking a long time
        // when the total count is actually present in each result page
        // make a PR to the upstream crate? or write custom http request by ourselves?
        let dependents = self.client.crate_reverse_dependencies(name)?.meta.total;

        let cratesio_report = CratesioReport {
            name: name.to_string(),
            is_hosted,
            downloads: crate_info.downloads,
            dependents,
        };

        Ok(cratesio_report)
    }
}