//! This module abstracts diff analysis between code versions

use anyhow::{anyhow, Result};
use camino::Utf8Path;
use flate2::read::GzDecoder;
use git2::{
    build::CheckoutBuilder, AutotagOption, Commit, Delta, Diff, DiffOptions, Direction,
    FetchOptions, IndexAddOption, Oid, Repository, Signature, Tree,
};
use regex::Regex;
use reqwest::blocking::Client;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::{
    collections::{HashMap, HashSet},
    fs::{read_dir, DirEntry, File},
    io::copy,
    path::{Path, PathBuf},
};
use tar::Archive;
use tempfile::{tempdir, TempDir};
use thiserror::Error;
use url::Url;
use walkdir::WalkDir;

use crate::super_toml::{CargoTomlParser, CargoTomlType};

/// This type presents information on the difference
/// between crates.io source code
/// and git source hosted code
/// for a given version
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct CrateSourceDiffReport {
    pub name: String,
    pub version: String,
    pub release_commit_found: Option<bool>,
    pub release_commit_analyzed: Option<bool>,
    pub is_different: Option<bool>,
    pub file_diff_stats: Option<FileDiffStats>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct FileDiffStats {
    pub files_added: HashSet<String>,
    pub files_modified: HashSet<String>,
    pub files_deleted: HashSet<String>,
}

pub struct DiffAnalyzer {
    dir: TempDir,   // hold temporary code files
    client: Client, // for downloading files
}

#[derive(Debug, Error)]
#[error("Head commit not found in the repository for {crate_name}:{version}")]
pub struct HeadCommitNotFoundError {
    crate_name: String,
    version: Version,
}

pub(crate) struct VersionDiffInfo<'a> {
    pub repo: &'a Repository,
    pub commit_a: Oid,
    pub commit_b: Oid,
    pub diff: Diff<'a>,
}

/// Trim down remote git urls like GitHub for cloning
/// e.g., cases where the crate is in a subdirectory of the repo
/// in the format "host_url/owner/repo"
pub(crate) fn trim_remote_url(url: &str) -> Result<String> {
    let url = Url::from_str(url)?;

    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("invalid host for {}", url))?;
    // TODO: check if host is from recognized sources, e.g. github, bitbucket, gitlab

    let mut segments = url
        .path_segments()
        .ok_or_else(|| anyhow!("error parsing url for {}", url))?;
    let owner = segments
        .next()
        .ok_or_else(|| anyhow!("repository url missing owner for {}", url))?;
    let repo = segments
        .next()
        .map(|repo| repo.trim_end_matches(".git"))
        .ok_or_else(|| anyhow!("repository url missing repo for {}", url))?;

    let url = format!("https://{}/{}/{}", host, owner, repo);
    Ok(url)
}

/// Given a directory
/// returns all paths for a given filename
pub(crate) fn get_all_paths_for_filename(dir_path: &Path, file_name: &str) -> Result<Vec<PathBuf>> {
    let mut file_paths: Vec<PathBuf> = Vec::new();
    for entry in WalkDir::new(dir_path).follow_links(true).into_iter() {
        let entry = entry?;
        let file = entry.file_name();
        let file = file
            .to_str()
            .ok_or_else(|| anyhow!("invalid unicode character in filename: {:?}", file))?;
        if file.ends_with(file_name) {
            file_paths.push(PathBuf::from(entry.path()));
        }
    }

    Ok(file_paths)
}

impl DiffAnalyzer {
    pub fn new() -> Result<Self> {
        Ok(Self {
            dir: tempdir()?,
            client: Client::new(),
        })
    }

    /// Given a crate version and its source repository,
    /// returns a report on differences between the source
    /// and code hosted on crates.io
    pub fn analyze_crate_source_diff(
        &self,
        name: &str,
        version: &str,
        repository: Option<&str>,
    ) -> Result<CrateSourceDiffReport> {
        // TODO make return type an Option
        // and return None when repository is not present
        let name = name.to_string();
        let version = version.to_string();

        let repository = match repository {
            Some(repo) => trim_remote_url(repo)?,
            None => {
                return Ok(CrateSourceDiffReport {
                    name,
                    version,
                    ..Default::default()
                });
            }
        };

        //Setup a git repository for crates.io hosted source code
        let crate_repo = self.get_git_repo_for_cratesio_version(&name, &version)?;
        let crate_repo_head = crate_repo.head()?.peel_to_commit()?;
        let cratesio_tree = crate_repo_head.tree()?;

        // Get commit for the version release in the git source
        let git_repo = self.get_git_repo(&name, &repository)?;
        // Keep track of the current state to reset before return
        let git_repo_starter_commit = git_repo.head()?.peel_to_commit()?;
        let head_commit_oid =
            match self.get_head_commit_oid_for_version(&git_repo, &name, &version)? {
                Some(commit) => commit,
                None => {
                    return Ok(CrateSourceDiffReport {
                        name,
                        version,
                        release_commit_found: Some(false),
                        ..Default::default()
                    });
                }
            };

        // Add git repo as a remote to crate repo
        self.setup_remote(&crate_repo, &repository, &head_commit_oid.to_string())?;

        // At this point, crate_repo contains crate.io hosted source with a single commit
        //                and git source as a remote
        // Therefore, we can get diff between crate_repo master and any remote commit

        // Get release version commit within crate repo
        let git_version_commit = crate_repo.find_commit(head_commit_oid)?;
        let crate_git_tree = git_version_commit.tree()?;

        // Get the tree for the crate directory path
        // e.g., when a repository contains multiple crates
        let mut checkout_builder = CheckoutBuilder::new();
        checkout_builder.force();
        git_repo.checkout_tree(
            git_repo.find_commit(head_commit_oid)?.tree()?.as_object(),
            Some(&mut checkout_builder),
        )?;
        let toml_path = match self.locate_package_toml(&git_repo, &name) {
            Ok(path) => path,
            Err(_e) => {
                return Ok(CrateSourceDiffReport {
                    name,
                    version,
                    release_commit_found: Some(true),
                    release_commit_analyzed: Some(false),
                    ..Default::default()
                });
            }
        };
        let toml_path = toml_path
            .parent()
            .ok_or_else(|| anyhow!("Fatal: toml path returned as root"))?;
        let crate_git_tree = self.get_subdirectory_tree(&crate_repo, &crate_git_tree, toml_path)?;

        let diff = crate_repo.diff_tree_to_tree(
            Some(&crate_git_tree),
            Some(&cratesio_tree),
            Some(&mut DiffOptions::new()),
        )?;

        let file_diff_stats = self.get_crate_source_file_diff_report(&diff)?;

        // reset repo
        git_repo.checkout_tree(
            git_repo_starter_commit.as_object(),
            Some(&mut checkout_builder),
        )?;

        Ok({
            CrateSourceDiffReport {
                name,
                version,
                release_commit_found: Some(true),
                release_commit_analyzed: Some(true),
                // Ignoring files from source not included in crates.io, possibly ignored
                is_different: Some(
                    !file_diff_stats.files_added.is_empty()
                        || !file_diff_stats.files_modified.is_empty(),
                ),
                file_diff_stats: Some(file_diff_stats),
            }
        })
    }

    pub(crate) fn get_git_repo_for_cratesio_version(
        &self,
        name: &str,
        version: &str,
    ) -> Result<Repository> {
        let path = self.get_cratesio_version(name, version)?;
        self.init_git(&path)
    }

    fn get_cratesio_version(&self, name: &str, version: &str) -> Result<PathBuf> {
        let download_path = format!(
            "https://crates.io/api/v1/crates/{}/{}/download",
            name, version
        );
        let dest_file = format!("{}-{}-cratesio", name, version);
        self.download_file(&download_path, &dest_file)
    }

    pub(crate) fn get_git_repo(&self, name: &str, url: &str) -> Result<Repository> {
        let dest_file = format!("{}-source", name);
        let dest_path = self.dir.path().join(&dest_file);
        if !dest_path.exists() {
            Repository::clone(url, &dest_path)?;
        }
        let repo = Repository::open(dest_path)?;
        Ok(repo)
    }

    fn get_repo_dir(&self, repo: &Repository) -> Result<PathBuf> {
        Ok(PathBuf::from(repo.path().parent().ok_or_else(|| {
            anyhow!("Fatal: .git file has no parent")
        })?))
    }

    fn download_file(&self, download_path: &str, dest_file: &str) -> Result<PathBuf> {
        // Destination directory to contain downloded files
        let dest_path = self.dir.path().join(&dest_file);

        // check if destination directory exists, if not proceed
        if !dest_path.exists() {
            // First download the file as tar_gz
            let targz_path = self.dir.path().join(format!("{}.targ.gz", dest_file));
            let mut targz_file = File::create(&targz_path)?;
            let mut response = self.client.get(download_path).send()?;
            copy(&mut response, &mut targz_file)?;

            // Then decompress the file
            self.decompress_targz(&targz_path, &dest_path)?;
        }

        // Get the only directory within dest_path where files are unpacked
        let entries: Vec<DirEntry> = read_dir(dest_path)?
            .filter_map(|entry| entry.ok())
            .collect();
        if entries.len() != 1 {
            return Err(anyhow!("Error in locating directory for unpacked files"));
        }

        // Return the directory containing unpacked files
        Ok(entries[0].path())
    }

    // note: in some functions, &self is not used,
    // however the functions may work with tempdirs set up by self,
    // therefore passing &self to them to make sure self (and, tempdir) still exists

    fn decompress_targz(&self, targz_path: &Path, dest_path: &Path) -> Result<()> {
        let tar_gz = File::open(targz_path)?;
        let tar = GzDecoder::new(tar_gz);
        let mut archive = Archive::new(tar);
        archive.unpack(dest_path)?;
        Ok(())
    }

    fn get_head_commit_oid_for_version(
        &self,
        repo: &Repository,
        name: &str,
        version: &str,
    ) -> Result<Option<Oid>> {
        // First try looking at repository tags
        if let Some(commit_oid) =
            self.get_head_commit_oid_for_version_from_tags(repo, name, version)?
        {
            Ok(Some(commit_oid))
        }
        // Else try parsing Cargo.toml histry
        else if let Some(commit_oid) =
            self.get_head_commit_oid_for_version_from_cargo_toml(repo, name, version)?
        {
            Ok(Some(commit_oid))
        } else {
            Ok(None)
        }
    }

    fn get_head_commit_oid_for_version_from_tags(
        &self,
        repo: &Repository,
        name: &str,
        version: &str,
    ) -> Result<Option<Oid>> {
        // Get candidate tags with a heuristic that tag will end with the version string
        let pattern = format!("*{}", version);
        let candidate_tags = repo.tag_names(Some(&pattern))?;

        let mut hm: HashMap<&str, Oid> = HashMap::new();
        for tag in candidate_tags.iter() {
            let tag = tag.ok_or_else(|| anyhow!("Error in fetching tags"))?;
            let commit = repo.revparse_single(tag)?.peel_to_commit()?;
            hm.insert(tag, commit.id());
        }

        // Now we check through a series of heuristics if tag matches a version
        let version_formatted_for_regex = version.replace('.', "\\.");
        let patterns = [
            // 1. Ensure the version part does not follow any digit between 1-9,
            // e.g., to distinguish betn 0.1.8 vs 10.1.8
            format!(r"^(?:.*[^1-9])?{}$", version_formatted_for_regex),
            // 2. If still more than one candidate,
            // check the extistence of crate name
            format!(r"^.*{}(?:.*[^1-9])?{}$", name, version_formatted_for_regex),
            // 3. check if  and only if crate name and version string is present
            // besides non-alphanumeric, e.g., to distinguish guppy vs guppy-summaries
            format!(r"^.*{}\W*{}$", name, version_formatted_for_regex),
        ];

        for pattern in &patterns {
            let re = Regex::new(pattern)?;

            // drain filter hashmap if tag matches the pattern
            let mut candidate_tags: Vec<&str> = Vec::new();
            for (tag, _oid) in hm.iter() {
                if !re.is_match(tag) {
                    candidate_tags.push(tag);
                }
            }
            for tag in candidate_tags {
                hm.remove(tag);
            }

            // multiple tags can point to the same commit
            let unique_commits: HashSet<Oid> = hm.values().cloned().collect();
            if unique_commits.len() == 1 {
                return Ok(Some(*unique_commits.iter().next().unwrap()));
            }
        }

        // TODO: add checking of changes in Cargo.toml file for a deterministic evaluation

        // If still failed to determine a single commit hash, return None
        Ok(None)
    }

    // Looks at each commit on Cargo.toml
    // to see if the commit updated version of the crate
    // to the input version
    // Note: we traverse each commit in the repo
    //     and check if the commit has touched Cargo.toml or not
    //     and if touches, perform a version update check
    //     which may be an expensive operation for large repos
    // However, a major use case of crate_source_diffing
    // is during dep updates where we query against the newer version of the crate
    // therefore, this function should find the desired commit early
    // while traversing from the head and
    // should be fast for practical use cases
    fn get_head_commit_oid_for_version_from_cargo_toml(
        &self,
        repo: &Repository,
        name: &str,
        version: &str,
    ) -> Result<Option<Oid>> {
        // keep track of current head to reset at the end of this function
        let starter_commit = repo.head()?.peel_to_commit()?;

        let mut checkout_builder = CheckoutBuilder::new();
        checkout_builder.force();

        let mut get_version_at_commit =
            |commit: &Commit| -> Result<Option<String>> {
                repo.checkout_tree(commit.as_object(), Some(&mut checkout_builder))?;
                if let Ok(toml_path) = self.locate_package_toml(repo, name) {
                    let toml_path = self.get_repo_dir(repo)?.join(toml_path);
                    Ok(Some(
                        CargoTomlParser::new(Utf8Path::from_path(&toml_path).ok_or_else(
                            || anyhow!("error converting {:?} to Utf8path", toml_path),
                        )?)?
                        .get_package_version()?,
                    ))
                } else {
                    Ok(None)
                }
            };

        let mut version_commit: Option<Oid> = None; // keep tracks of output commit

        // git2 does not provide a wrapper for `git log --follow`
        // In order to find commits touching Cargo.toml
        // We take inspiration from this code -
        // https://github.com/rust-lang/git2-rs/issues/588#issuecomment-856757971
        let mut revwalk = repo.revwalk()?;
        revwalk.set_sorting(git2::Sort::TIME)?;
        revwalk.push_head()?;
        for commit_oid in revwalk {
            let commit_oid = commit_oid?;
            let commit = repo.find_commit(commit_oid)?;
            if commit.parent_count() > 1 {
                // Ignore merge commits (2+ parents)
                continue;
            }

            let tree = commit.tree()?;
            if commit.parent_count() == 1 {
                let prev_commit = commit.parent(0)?;
                let prev_tree = prev_commit.tree()?;
                let diff = repo.diff_tree_to_tree(Some(&prev_tree), Some(&tree), None)?;

                for delta in diff.deltas() {
                    if delta
                        .new_file()
                        .path()
                        .unwrap_or_else(|| Path::new(""))
                        .ends_with("Cargo.toml")
                    {
                        if let Some(post_version) = get_version_at_commit(&commit)? {
                            if post_version == version {
                                if let Some(prior_version) = get_version_at_commit(&prev_commit)? {
                                    if Version::from_str(&post_version)
                                        > Version::from_str(&prior_version)
                                    {
                                        // Case 1: version updated
                                        version_commit = Some(commit_oid);
                                    }
                                } else {
                                    // case 2: Cargo.toml added or package renamed
                                    version_commit = Some(commit_oid);
                                }
                            }
                        }
                    }
                }
            } else {
                // case 3: Initial commit
                if let Some(post_version) = get_version_at_commit(&commit)? {
                    if post_version == version {
                        version_commit = Some(commit_oid);
                    }
                }
            }
        }
        // case 4: could not found and the version commit remains None

        // reset head before return
        repo.checkout_tree(starter_commit.as_object(), Some(&mut checkout_builder))?;
        Ok(version_commit)
    }

    fn init_git(&self, path: &Path) -> Result<Repository> {
        // initiates a git repository in the path
        let repo = Repository::init(path)?;

        // add and commit existing files
        let mut index = repo.index()?;
        index.add_all(["*"].iter(), IndexAddOption::DEFAULT, None)?;
        let oid = index.write_tree()?;
        let signature = Signature::now("user", "email@domain.com")?;
        let tree = repo.find_tree(oid)?;
        repo.commit(
            Some("HEAD"),     // point HEAD to new commit
            &signature,       // author
            &signature,       // committer
            "initial commit", // commit message
            &tree,            // tree
            &[],              // initial commit
        )?;

        Ok(Repository::open(path)?)
    }

    fn setup_remote(&self, repo: &Repository, url: &str, fetch_commit: &str) -> Result<()> {
        // Connect to remote
        let remote_name = "source";
        let mut remote = repo.remote(remote_name, url)?;
        remote.connect(Direction::Fetch)?;

        // Get default branch
        let default = remote.default_branch()?;
        let default = default
            .as_str()
            .ok_or_else(|| anyhow!("No default branch found"))?;

        // Fetch all tags
        let mut fetch_options = FetchOptions::new();
        fetch_options.download_tags(AutotagOption::All);

        // Fetch data
        remote.fetch(&[default, fetch_commit], Some(&mut fetch_options), None)?;

        Ok(())
    }

    /// The repository of a crate may or may not contain multiple crates
    /// Given a crate name and its repository
    /// This function returns the path to Cargo.toml for the given crate
    pub fn locate_package_toml(&self, repo: &Repository, name: &str) -> Result<PathBuf> {
        let repo_dir = self.get_repo_dir(repo)?;
        let toml_paths = get_all_paths_for_filename(&repo_dir, "Cargo.toml")?;
        for path in &toml_paths {
            let toml_parser = CargoTomlParser::new(
                Utf8Path::from_path(path)
                    .ok_or_else(|| anyhow!("invalid unicode in path: {:?}", path))?,
            )?;
            if matches!(toml_parser.get_toml_type()?, CargoTomlType::Package)
                && toml_parser.get_package_name()? == name
            {
                return Ok(path.strip_prefix(&repo_dir)?.to_path_buf());
            }
        }

        Err(anyhow!(
            "Cargo.toml could not be located for {} in {:?}",
            name,
            repo.path()
        ))
    }

    fn get_subdirectory_tree<'a>(
        &self,
        repo: &'a Repository,
        tree: &'a Tree,
        path: &Path,
    ) -> Result<Tree<'a>> {
        if path.file_name().is_none() {
            // Root of the repository path marked by an empty string
            return Ok(tree.clone());
        }
        let tree = tree.get_path(path)?.to_object(repo)?.id();
        let tree = repo.find_tree(tree)?;
        Ok(tree)
    }

    fn get_crate_source_file_diff_report(&self, diff: &Diff) -> Result<FileDiffStats> {
        let mut files_added: HashSet<String> = HashSet::new();
        let mut files_modified: HashSet<String> = HashSet::new();
        let mut files_deleted: HashSet<String> = HashSet::new();

        // Ignore below files as they are changed whenever publishing to crates.io
        // TODO: compare Cargo.toml.orig in crates.io with Cargo.toml in git
        let ignore_paths: HashSet<&str> = vec![
            ".cargo_vcs_info.json",
            "Cargo.toml",
            "Cargo.toml.orig",
            "Cargo.lock",
            "README.md",
            "CHANGELOG.md",
            "LICENSE.md",
            "LICENSE-MIT",
            "LICENSE-APACHE",
            "crates-io.md",
        ]
        .into_iter()
        .collect();

        for diff_delta in diff.deltas() {
            let path = diff_delta
                .new_file()
                .path()
                .or_else(|| diff_delta.old_file().path())
                .ok_or_else(|| anyhow!("no file path for {:?}", diff_delta))?
                .to_str()
                .ok_or_else(|| anyhow!("path error in git diff"))?
                .to_string();
            if ignore_paths.contains(path.as_str()) {
                continue;
            }

            // TODO: Many times files like README are added/modified
            // by having only a single line in crates.io and deleting original contents
            // Also, we need to distinguish non source-code file here
            // to avoid noise in warning
            match diff_delta.status() {
                Delta::Added => {
                    files_added.insert(path);
                }
                Delta::Modified => {
                    // modification counts modified file as 2 files
                    files_modified.insert(path);
                }
                Delta::Deleted => {
                    files_deleted.insert(path);
                }
                _ => (),
            }
        }

        Ok(FileDiffStats {
            files_added,
            files_modified,
            files_deleted,
        })
    }

    pub(crate) fn get_git_source_version_diff_info<'a>(
        &'a self,
        name: &str,
        repo: &'a Repository,
        version_a: &Version,
        version_b: &Version,
    ) -> Result<VersionDiffInfo<'a>> {
        // TODO: This function works only in cases where the root directory
        // of the git repository contains a Cargo.toml file
        let toml_path = self.locate_package_toml(repo, name)?;
        let toml_path = toml_path
            .parent()
            .ok_or_else(|| anyhow!("Cannot find crate directory"))?;

        let commit_oid_a = self
            .get_head_commit_oid_for_version(repo, name, &version_a.to_string())?
            .ok_or_else(|| HeadCommitNotFoundError {
                crate_name: name.to_string(),
                version: version_a.clone(),
            })?;
        let tree_a = repo.find_commit(commit_oid_a)?.tree()?;
        let tree_a = self.get_subdirectory_tree(repo, &tree_a, toml_path)?;

        let commit_oid_b = self
            .get_head_commit_oid_for_version(repo, name, &version_b.to_string())?
            .ok_or_else(|| HeadCommitNotFoundError {
                crate_name: name.to_string(),
                version: version_b.clone(),
            })?;
        let tree_b = repo.find_commit(commit_oid_b)?.tree()?;
        let tree_b = self.get_subdirectory_tree(repo, &tree_b, toml_path)?;

        let diff =
            repo.diff_tree_to_tree(Some(&tree_a), Some(&tree_b), Some(&mut DiffOptions::new()))?;

        Ok(VersionDiffInfo {
            repo,
            commit_a: commit_oid_a,
            commit_b: commit_oid_b,
            diff,
        })
    }

    // This method takes two local repositories as input,
    //     Presumably two different versions of the same code base initiated in different repos
    //     For example, when comparing code for two versions of a crate hosted on crates.io;
    // and returns VersionDiffInfo between the heads of the two repositories
    pub(crate) fn get_version_diff_info_between_repos<'a>(
        &'a self,
        repo_version_a: &'a Repository,
        repo_version_b: &Repository,
    ) -> Result<VersionDiffInfo<'a>> {
        let version_a_commit = repo_version_a.head()?.peel_to_commit()?;
        let version_a_tree = version_a_commit.tree()?;

        // make repo_b a branch of repo_a
        let head_b = repo_version_b.head()?.peel_to_commit()?;
        self.setup_remote(
            repo_version_a,
            repo_version_b
                .path()
                .to_str()
                .ok_or_else(|| anyhow!("no local path found for repository"))?,
            &head_b.id().to_string(),
        )?;

        // Get head commit for repo_b branch on repo_a
        let version_b_commit = repo_version_a.find_commit(head_b.id())?;
        let version_b_tree = version_b_commit.tree()?;

        let diff = repo_version_a.diff_tree_to_tree(
            Some(&version_a_tree),
            Some(&version_b_tree),
            Some(&mut DiffOptions::new()),
        )?;

        Ok(VersionDiffInfo {
            repo: repo_version_a,
            commit_a: version_a_commit.id(),
            commit_b: version_b_commit.id(),
            diff,
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use guppy::{graph::PackageGraph, MetadataCommand};
    use once_cell::sync::Lazy;
    use serial_test::serial;
    use std::sync::Once;

    static GRAPH_VALID_DEP: Lazy<PackageGraph> = Lazy::new(|| {
        MetadataCommand::new()
            .current_dir(PathBuf::from("resources/test/valid_dep"))
            .build_graph()
            .unwrap()
    });

    static DIFF_ANALYZER: Lazy<DiffAnalyzer> = Lazy::new(|| DiffAnalyzer::new().unwrap());

    static INIT_GIT_REPOS: Once = Once::new();
    pub fn setup_git_repos() {
        // Multiple tests work with common git repos.
        // As git2::Repositroy mutable reference is not thread safe,
        // we'd need to run those tests serially.
        // However, in this function, we clone those common repos
        // to avoid redundant set up within the tests
        INIT_GIT_REPOS.call_once(|| {
            let name = "guppy";
            let url = "https://github.com/facebookincubator/cargo-guppy";
            DIFF_ANALYZER.get_git_repo(name, url).unwrap();

            let name = "octocrab";
            let url = "https://github.com/XAMPPRocky/octocrab";
            DIFF_ANALYZER.get_git_repo(name, url).unwrap();
        });
    }

    fn get_test_diff_analyzer() -> DiffAnalyzer {
        DiffAnalyzer::new().unwrap()
    }

    #[test]
    fn test_diff_trim_git_url() {
        let url = "https://github.com/facebookincubator/cargo-guppy/tree/main/guppy";
        let trimmed_url = trim_remote_url(url).unwrap();
        assert_eq!(
            trimmed_url,
            "https://github.com/facebookincubator/cargo-guppy"
        );
    }

    #[test]
    fn test_diff_download_file() {
        let diff_analyzer = get_test_diff_analyzer();
        let name = "criterion-cpu-time";
        let version = "0.1.0";
        let path = diff_analyzer
            .download_file(
                format!(
                    "https://crates.io//api/v1/crates/{}/{}/download",
                    name, version
                )
                .as_str(),
                format!("{}-{}", &name, &version).as_str(),
            )
            .unwrap();
        assert!(path.exists());
    }

    #[test]
    fn test_diff_setup_crate_source_diff_analyzer() {
        let diff_analyzer = get_test_diff_analyzer();
        let name = "syn";
        let version = "0.15.44";
        let path = diff_analyzer.get_cratesio_version(name, version).unwrap();
        assert!(path.exists());

        let repo = diff_analyzer.init_git(&path).unwrap();
        assert!(repo.path().exists());
        let commit = repo.head().unwrap().peel_to_commit();
        assert!(commit.is_ok());

        // Add git repo as a remote to crate repo
        let url = "https://github.com/dtolnay/syn";
        let fetch_commit = "6d798b63c255e90b7b1dbbfb3707fdce1704a18d";
        diff_analyzer
            .setup_remote(&repo, url, fetch_commit)
            .unwrap();
    }

    #[test]
    fn test_diff_git_repo() {
        let diff_analyzer = get_test_diff_analyzer();
        let name = "criterion-cpu-time";
        let url = "https://github.com/YangKeao/criterion-cpu-time";
        let repo = diff_analyzer.get_git_repo(name, url).unwrap();
        assert!(repo.workdir().is_some());
        assert!(repo.path().exists());
    }

    #[test]
    fn test_diff_head_commit_oid_for_version_from_tags() {
        let diff_analyzer = get_test_diff_analyzer();
        let name = "test-version-tag";
        let url = "https://github.com/nasifimtiazohi/test-version-tag";

        let repo = diff_analyzer.get_git_repo(name, url).unwrap();
        let oid = diff_analyzer
            .get_head_commit_oid_for_version_from_tags(&repo, name, "0.0.8")
            .unwrap();
        assert!(oid.is_none());
        let oid = diff_analyzer
            .get_head_commit_oid_for_version_from_tags(&repo, name, "10.0.8")
            .unwrap();
        assert_eq!(
            oid.unwrap(),
            Oid::from_str("51efd612af12183a682bb3242d41369d2879ad60").unwrap()
        );
        let oid = diff_analyzer
            .get_head_commit_oid_for_version_from_tags(&repo, name, "10.0.8-")
            .unwrap();
        assert!(oid.is_none());

        let oid = diff_analyzer
            .get_head_commit_oid_for_version_from_tags(&repo, "hakari", "0.3.0")
            .unwrap();
        assert_eq!(
            oid.unwrap(),
            Oid::from_str("946ddf053582067b843c19f1270fe92eaa0a7cb3").unwrap()
        );
        let oid = diff_analyzer
            .get_head_commit_oid_for_version_from_tags(&repo, "guppy", "0.3.0")
            .unwrap();
        assert_eq!(
            oid.unwrap(),
            Oid::from_str("dd7e5609e640f468a7e15a32fe36b607bae13e3e").unwrap()
        );
        let oid = diff_analyzer
            .get_head_commit_oid_for_version_from_tags(&repo, "guppy-summaries", "0.3.0")
            .unwrap();
        assert_eq!(
            oid.unwrap(),
            Oid::from_str("24e00d39f90baa1daa2ef6f9a2bdb49e581874b3").unwrap()
        );
    }

    #[test]
    #[serial]
    fn test_diff_locate_cargo_toml() {
        setup_git_repos();

        let name = "guppy";
        let url = "https://github.com/facebookincubator/cargo-guppy";
        let repo = DIFF_ANALYZER.get_git_repo(name, url).unwrap();
        let path = DIFF_ANALYZER.locate_package_toml(&repo, name).unwrap();
        assert_eq!("guppy/Cargo.toml", path.to_str().unwrap());

        let name = "octocrab";
        let url = "https://github.com/XAMPPRocky/octocrab";
        let repo = DIFF_ANALYZER.get_git_repo(name, url).unwrap();
        let path = DIFF_ANALYZER.locate_package_toml(&repo, name).unwrap();
        assert_eq!("Cargo.toml", path.to_str().unwrap());
    }

    #[test]
    #[serial]
    fn test_diff_get_subdirectory_tree() {
        setup_git_repos();
        let name = "guppy";
        let url = "https://github.com/facebookincubator/cargo-guppy";
        let repo = DIFF_ANALYZER.get_git_repo(name, url).unwrap();
        let tree = repo
            .find_commit(Oid::from_str("dc6dcc151821e787ac02379bcd0319b26c962f55").unwrap())
            .unwrap()
            .tree()
            .unwrap();
        let path = PathBuf::from("guppy");
        let subdirectory_tree = DIFF_ANALYZER
            .get_subdirectory_tree(&repo, &tree, &path)
            .unwrap();
        assert_ne!(tree.id(), subdirectory_tree.id());
        // TODO: test that subdir tree doesn't have files from cargo-guppy
    }

    #[test]
    #[serial]
    fn test_diff_crate_source_diff_analyzer() {
        setup_git_repos();
        let graph = &GRAPH_VALID_DEP;

        for package in graph.packages() {
            if package.name() == "guppy" {
                println!("testing {}, {}", package.name(), package.version());
                let report = DIFF_ANALYZER
                    .analyze_crate_source_diff(
                        package.name(),
                        &package.version().to_string(),
                        package.repository(),
                    )
                    .unwrap();
                if report.release_commit_found.is_none()
                    || !report.release_commit_found.unwrap()
                    || !report.release_commit_analyzed.unwrap()
                {
                    continue;
                }

                assert!(report.file_diff_stats.is_some());
                println!("{:?}", report);

                if package.name() == "guppy" {
                    assert!(!report.is_different.unwrap());
                }
            }
        }
    }

    #[test]
    #[serial]
    fn test_diff_version_diff() {
        setup_git_repos();

        let name = "guppy";
        let repository = "https://github.com/facebookincubator/cargo-guppy";

        let repo = DIFF_ANALYZER.get_git_repo(name, repository).unwrap();
        let version_diff_info = DIFF_ANALYZER
            .get_git_source_version_diff_info(
                name,
                &repo,
                &Version::parse("0.8.0").unwrap(),
                &Version::parse("0.9.0").unwrap(),
            )
            .unwrap();

        assert_eq!(
            version_diff_info.commit_a,
            Oid::from_str("dc6dcc151821e787ac02379bcd0319b26c962f55").unwrap()
        );
        assert_eq!(
            version_diff_info.commit_b,
            Oid::from_str("fe61a8b85feab1963ee1985bf0e4791fdd354aa5").unwrap()
        );

        let diff = version_diff_info.diff;
        assert_eq!(diff.stats().unwrap().files_changed(), 6);
        assert_eq!(diff.stats().unwrap().insertions(), 199);
        assert_eq!(diff.stats().unwrap().deletions(), 82);
    }

    #[test]
    #[serial]
    fn test_diff_version_diff_from_crates_io() {
        let diff_analyzer = get_test_diff_analyzer();
        let name = "guppy";
        let version_a = "0.8.0";
        let version_b = "0.9.0";

        let repo_a = diff_analyzer
            .get_git_repo_for_cratesio_version(name, version_a)
            .unwrap();
        let repo_b = diff_analyzer
            .get_git_repo_for_cratesio_version(name, version_b)
            .unwrap();

        let version_diff_info = diff_analyzer
            .get_version_diff_info_between_repos(&repo_a, &repo_b)
            .unwrap();

        let diff = version_diff_info.diff;
        assert_eq!(diff.stats().unwrap().files_changed(), 9);
        assert_eq!(diff.stats().unwrap().insertions(), 244);
        assert_eq!(diff.stats().unwrap().deletions(), 179);
    }

    #[test]
    #[serial]
    fn test_diff_head_commit_not_found_error() {
        setup_git_repos();

        let name = "guppy";
        let repository = "https://github.com/facebookincubator/cargo-guppy";

        let repo = DIFF_ANALYZER.get_git_repo(name, repository).unwrap();
        let diff = DIFF_ANALYZER
            .get_git_source_version_diff_info(
                name,
                &repo,
                &Version::parse("0.0.0").unwrap(),
                &Version::parse("0.9.0").unwrap(),
            )
            .map_err(|error| {
                error
                    .root_cause()
                    .downcast_ref::<HeadCommitNotFoundError>()
                    // If not the error type, downcast will be None
                    .is_none()
            })
            .err()
            .unwrap();
        assert!(!diff);
    }

    #[test]
    fn test_diff_get_all_paths_for_filename() {
        let paths = get_all_paths_for_filename(Path::new("."), "Cargo.toml").unwrap();
        assert_eq!(5, paths.len());
        assert!(paths.contains(&PathBuf::from("./Cargo.toml")));
        assert!(paths.contains(&PathBuf::from("./resources/test/valid_dep/Cargo.toml")));
    }

    #[test]
    fn test_diff_head_commit_oid_from_cargo_toml() {
        let diff_analyzer = get_test_diff_analyzer();
        let name = "unicase";
        let url = "https://github.com/seanmonstar/unicase";
        let repo = diff_analyzer.get_git_repo(name, url).unwrap();

        // Case 1: Version updated
        let commit = diff_analyzer
            .get_head_commit_oid_for_version_from_cargo_toml(&repo, name, "2.5.1")
            .unwrap()
            .unwrap();
        assert_eq!(
            commit,
            Oid::from_str("141699ceaf145621eea41ce7597d3ade42902c37").unwrap()
        );

        // Case 2: Package Cargo.toml added (renamed in this case)
        let commit = diff_analyzer
            .get_head_commit_oid_for_version_from_cargo_toml(&repo, name, "0.0.1")
            .unwrap()
            .unwrap();
        assert_eq!(
            commit,
            Oid::from_str("5834ee501c350ce5db5d1e62df3b7c207219a803").unwrap()
        );

        // Case 3: Initial commit
        let commit = diff_analyzer
            .get_head_commit_oid_for_version_from_cargo_toml(&repo, "case", "0.0.1")
            .unwrap()
            .unwrap();
        assert_eq!(
            commit,
            Oid::from_str("1236f7b92854174eba20b5d3a13aaeb5a34a6bff").unwrap()
        );

        // Case 4: Commit not found
        let commit = diff_analyzer
            .get_head_commit_oid_for_version_from_cargo_toml(&repo, name, "0.0.0")
            .unwrap();
        assert!(commit.is_none());
    }

    #[test]
    fn test_diff_head_commit_oid() {
        let diff_analyzer = get_test_diff_analyzer();
        let name = "unicase";
        let url = "https://github.com/seanmonstar/unicase";
        let repo = diff_analyzer.get_git_repo(name, url).unwrap();

        // Case 1: Tag exists
        let commit = diff_analyzer
            .get_head_commit_oid_for_version(&repo, name, "2.4.0")
            .unwrap()
            .unwrap();
        assert_eq!(
            commit,
            Oid::from_str("8a93c809b061615bfa1021e9ab3bd115b8f3b1cc").unwrap()
        );

        // Case 2: Tag doesn't exist
        let commit = diff_analyzer
            .get_head_commit_oid_for_version(&repo, name, "0.0.5")
            .unwrap()
            .unwrap();
        assert_eq!(
            commit,
            Oid::from_str("dc1fa6bad26f0f40f415146fb581a928e214981a").unwrap()
        );
    }
}
