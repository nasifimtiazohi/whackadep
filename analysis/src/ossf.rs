//! This module abstracts fetching OSSF reports
//! reference: https://github.com/ossf/scorecard
//! Currently it downloads the full latest data
//! We can replace this by querying Google BigQuery service

use anyhow::Result;
use guppy::graph::{PackageGraph, PackageMetadata};
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, collections::HashSet, fs::File};
use tempfile::{tempdir};
use reqwest::blocking::Client;

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct PackageOSSFReport {
    pub name: String,
    pub ossf_report: Option<OSSFReport>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct OSSFReport {
    pub security_policy: bool,
    pub multi_org_contributors: bool,
    pub frozen_deps: bool,
    pub signed_releases: bool,
    pub signed_tags: bool,
    pub ci_tests: bool,
    pub code_review: bool,
    pub cii_best_practices: bool,
    pub pull_requests: bool,
    pub fuzzing: bool,
    pub sast: bool,
    pub active: bool,
    pub branch_protection: bool,
    pub packaging: bool,
}

pub struct OSSFClient {
    packages: RefCell<HashSet<String>>,
}

impl OSSFClient {
    pub fn new() -> Self {
        Self {
            packages: RefCell::new(HashSet::new()),
        }
    }

    pub fn get_ossf_reports(self, graph: &PackageGraph) -> Result<()> {
        // Get direct dependencies
        let direct_dependencies: Vec<PackageMetadata> = graph
            .query_workspace()
            .resolve_with_fn(|_, link| {
                let (from, to) = link.endpoints();
                from.in_workspace() && !to.in_workspace()
            })
            .packages(guppy::graph::DependencyDirection::Forward)
            .filter(|pkg| !pkg.in_workspace())
            .collect();

        // put direct dependencies into hashset for quick lookup
        for package in &direct_dependencies {
            self.packages
                .borrow_mut()
                .insert(package.name().to_string());
        }

        Ok(())
    }

    fn download_latest_ossf_data() -> Result<()> {
        // let download_url = "https://storage.googleapis.com/ossf-scorecards/latest.json";
        // let client = Client::new();

        // let dir = tempdir()?;
        // let dest_path = dir.path().join("ossf-latest.json");
        // let mut file = File::create(&dest_path)?;
        // let mut response = client.get(download_url).send()?;
        // copy(&mut response, &mut file);

        let download_url = "https://storage.googleapis.com/ossf-scorecards/latest.json";
        let client = Client::new();
        let response = client.get(download_url).send()?;
        let response = response.json()?;
        println!("{:?}",response);

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use guppy::{graph::PackageGraph, MetadataCommand};
    use std::path::PathBuf;

    fn get_test_graph() -> PackageGraph {
        MetadataCommand::new()
            .current_dir(PathBuf::from("resources/test/valid_dep"))
            .build_graph()
            .unwrap()
    }

    #[test]
    fn test_ossf_client() {
        let graph = get_test_graph();
        let ossf_client = OSSFClient::new();
        ossf_client.get_ossf_reports(&graph).unwrap();
    }

    #[test]
    fn test_ossf_download() {
        OSSFClient::download_latest_ossf_data().unwrap();
    }
}
