//! This module abstracts the communication with crates.io for a given crate
//! Returns Error if the crate is not hosted on crates_io

// TODO: A cheaper way to interact with crates.io can be working with their
// experimental database dump that is updated daily, https://crates.io/data-access,
// which will enable us to avoid making http requests and dealing with rate limits

// TODO: While we use crates_io_api crate
// some calls are cheaper if we make http request by ourselves
// as the crate has no direct API for our requirements and will make many extra calls

use anyhow::{anyhow, Result};
use guppy::graph::PackageMetadata;
use semver::Version;
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
pub struct CratesioReport {
    pub name: String,
    pub is_hosted: bool,
    pub downloads: u64,
    pub dependents: u64, // Direct dependents
}

pub struct CratesioAnalyzer {
    crates_io_api_client: crates_io_api::SyncClient,
    http_client: reqwest::blocking::Client,
}

impl CratesioAnalyzer {
    pub fn new() -> Result<Self> {
        Ok(Self {
            crates_io_api_client: crates_io_api::SyncClient::new(
                "User-Agent: Whackadep (https://github.com/diem/whackadep)",
                std::time::Duration::from_millis(1000),
            )?,
            http_client: reqwest::blocking::Client::builder()
                .user_agent("diem/whackadep")
                .build()?,
        })
    }

    pub fn analyze_cratesio(self, package: &PackageMetadata) -> Result<CratesioReport> {
        let name = package.name();
        let is_hosted = package.source().is_crates_io();
        self.get_cratesio_metrics(name, is_hosted)
    }

    pub fn get_cratesio_metrics(&self, name: &str, is_hosted: bool) -> Result<CratesioReport> {
        if !is_hosted {
            return Ok(CratesioReport {
                name: name.to_string(),
                is_hosted,
                ..Default::default()
            });
        }

        let crate_info = self.crates_io_api_client.get_crate(name)?.crate_data;
        let dependents = self.get_total_dependents(name)?;

        let cratesio_report = CratesioReport {
            name: name.to_string(),
            is_hosted,
            downloads: crate_info.downloads,
            dependents,
        };

        Ok(cratesio_report)
    }

    pub fn get_total_dependents(&self, crate_name: &str) -> Result<u64> {
        let api_endpoint = format!(
            "https://crates.io/api/v1/crates/{}/reverse_dependencies",
            crate_name
        );

        let response = self.http_client.get(api_endpoint).send()?;
        if !response.status().is_success() {
            return Err(anyhow!("http request to Crates.io failed: {:?}", response));
        }

        let response: serde_json::Value = response.json()?;
        let dependents: u64 = response["meta"]["total"]
            .as_u64()
            .ok_or_else(|| anyhow!("total dependents is not an integer"))?;

        Ok(dependents)
    }

    pub fn get_version_downloads(&self, crate_name: &str, version: &Version) -> Result<u64> {
        let api_endpoint = format!("https://crates.io/api/v1/crates/{}/{}", crate_name, version);

        let response = self.http_client.get(api_endpoint).send()?;
        if !response.status().is_success() {
            return Err(anyhow!("http request to Crates.io failed: {:?}", response));
        }

        let response: serde_json::Value = response.json()?;
        let downloads: u64 = response["version"]["downloads"]
            .as_u64()
            .ok_or_else(|| anyhow!("version downloads is not an integer"))?;

        Ok(downloads)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use guppy::MetadataCommand;
    use std::path::PathBuf;

    fn test_cratesio_analyzer() -> CratesioAnalyzer {
        CratesioAnalyzer::new().unwrap()
    }

    #[test]
    fn test_cratesio_stats_for_libc() {
        let cratesio_analyzer = test_cratesio_analyzer();

        let graph = MetadataCommand::new()
            .current_dir(PathBuf::from("resources/test/valid_dep"))
            .build_graph()
            .unwrap();

        let libc = graph.packages().find(|p| p.name() == "libc").unwrap();
        let report = cratesio_analyzer.analyze_cratesio(&libc).unwrap();

        assert!(report.is_hosted);
        assert!(report.downloads > 0);
        assert!(report.dependents > 0);
    }

    #[test]
    fn test_cratesio_stats_for_unhosted_crate_name() {
        let cratesio_analyzer = test_cratesio_analyzer();
        let report = cratesio_analyzer
            .get_cratesio_metrics("unhosted_crate", false)
            .unwrap();

        assert_eq!(report.downloads, 0);
        assert_eq!(report.dependents, 0);
    }

    #[test]
    fn test_cratesio_version_downloads() {
        let cratesio_analyzer = test_cratesio_analyzer();
        let downloads = cratesio_analyzer
            .get_version_downloads("guppy", &Version::parse("0.8.0").unwrap())
            .unwrap();
        assert!(downloads > 10000);
    }
}
