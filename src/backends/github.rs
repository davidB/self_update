use std::env;
use std::path::PathBuf;
use std::cmp;
use std::fs;
use std::io::Write;

use serde_json;
use reqwest;
use tempdir;

use super::super::replace_exe;
use super::super::extract_targz;
use super::super::prompt_ok;
use super::super::download_to_file_with_progress;
use super::super::errors::*;


#[derive(Debug)]
struct ReleaseAsset {
    download_url: String,
    name: String,
}
impl ReleaseAsset {
    /// Parse a release-asset json object
    ///
    /// Errors:
    ///     * Missing required name & download-url keys
    fn from_asset(asset: &serde_json::Value) -> Result<ReleaseAsset> {
        let download_url = asset["browser_download_url"].as_str()
            .ok_or_else(|| format_err!(Error::Update, "Asset missing `browser_download_url`"))?;
        let name = asset["name"].as_str()
            .ok_or_else(|| format_err!(Error::Update, "Asset missing `name`"))?;
        Ok(ReleaseAsset {
            download_url: download_url.to_owned(),
            name: name.to_owned(),
        })
    }
}


/// `github::Updater` builder
pub struct Builder {
    repo_owner: Option<String>,
    repo_name: Option<String>,
    target: Option<String>,
    bin_name: Option<String>,
    bin_install_path: Option<PathBuf>,
    bin_path_in_tarball: Option<PathBuf>,
    show_progress: bool,
    current_version: Option<String>,
}
impl Builder {
    /// Initialize a new builder, defaulting the `bin_install_path` to the current
    /// executable's path
    ///
    /// * Errors:
    ///     * Io - Determining current exe path
    pub fn new() -> Result<Builder> {
        Ok(Builder {
            repo_owner: None, repo_name: None,
            target: None, bin_name: None,
            bin_install_path: Some(env::current_exe()?),
            bin_path_in_tarball: None,
            show_progress: false,
            current_version: None,
        })
    }

    /// Set the repo owner, used to build a github api url
    pub fn repo_owner(&mut self, owner: &str) -> &mut Self {
        self.repo_owner = Some(owner.to_owned());
        self
    }

    /// Set the repo name, used to build a github api url
    pub fn repo_name(&mut self, name: &str) -> &mut Self {
        self.repo_name = Some(name.to_owned());
        self
    }

    /// Set the current app version, used to compare against the latest available version.
    /// The `crate_version!` macro can be used to pull the version from your `Cargo.toml`
    pub fn current_version(&mut self, ver: &str) -> &mut Self {
        self.current_version = Some(ver.to_owned());
        self
    }

    /// Set the target triple that will be downloaded, e.g. `x86_64-unknown-linux-gnu`.
    /// The `get_target` function can cover use cases for most mainstream arches
    pub fn target(&mut self, target: &str) -> &mut Self {
        self.target = Some(target.to_owned());
        self
    }

    /// Set the exe's name
    pub fn bin_name(&mut self, name: &str) -> &mut Self {
        self.bin_name = Some(name.to_owned());
        if self.bin_path_in_tarball.is_none() {
            self.bin_path_in_tarball = Some(PathBuf::from(name));
        }
        self
    }

    /// Set the installation path for the new exe, defaults to the current
    /// executable's path
    pub fn bin_install_path(&mut self, bin_install_path: &str) -> &mut Self {
        self.bin_install_path = Some(PathBuf::from(bin_install_path));
        self
    }

    /// Set the path of the exe inside the release tarball. This is the location
    /// of the executable relative to the base of the tar'd directory and is the
    /// path that will be copied to the `bin_install_path`.
    ///
    /// # Example
    ///
    /// For a tarball `myapp.tar.gz` with the contents:
    ///
    /// ```shell
    /// myapp/
    ///  |--- myapp  # <-- executable
    /// ```
    ///
    /// The path provided should be:
    ///
    /// ```rust,ignore
    /// Builder::configure()?
    ///     ....
    ///     .bin_install_path("myapp")
    /// ```
    pub fn bin_path_in_tarball(&mut self, bin_path: &str) -> &mut Self {
        self.bin_path_in_tarball = Some(PathBuf::from(bin_path));
        self
    }

    /// Toggle download progress bar, defaults to off.
    pub fn show_progress(&mut self, show: bool) -> &mut Self {
        self.show_progress = show;
        self
    }

    /// Confirm config and create a ready-to-use `Updater`
    ///
    /// * Errors:
    ///     * Config - Invalid `Updater` configuration
    pub fn build(&self) -> Result<Updater> {
        Ok(Updater {
            repo_owner: if let Some(ref owner) = self.repo_owner { owner.to_owned() } else { bail!(Error::Config, "`repo_owner` required")},
            repo_name: if let Some(ref name) = self.repo_name { name.to_owned() } else { bail!(Error::Config, "`repo_name` required")},
            target: if let Some(ref target) = self.target { target.to_owned() } else { bail!(Error::Config, "`target` required")},
            bin_name: if let Some(ref name) = self.bin_name { name.to_owned() } else { bail!(Error::Config, "`bin_name` required")},
            bin_install_path: if let Some(ref path) = self.bin_install_path { path.to_owned() } else { bail!(Error::Config, "`bin_install_path` required")},
            bin_path_in_tarball: if let Some(ref path) = self.bin_path_in_tarball { path.to_owned() } else { bail!(Error::Config, "`bin_path_in_tarball` required")},
            current_version: if let Some(ref ver) = self.current_version { ver.to_owned() } else { bail!(Error::Config, "`current_version` required")},
            show_progress: self.show_progress,
        })
    }
}


/// Updater intended for handling releases distributed via GitHub
pub struct Updater {
    repo_owner: String,
    repo_name: String,
    target: String,
    current_version: String,
    bin_name: String,
    bin_install_path: PathBuf,
    bin_path_in_tarball: PathBuf,
    show_progress: bool,
}
impl Updater {
    /// Initialize a new `Updater` builder
    pub fn configure() -> Result<Builder> {
        Builder::new()
    }

    /// Update the current binary to the latest release
    pub fn update(self) -> Result<()> {
        // Make sure openssl can find required files
        #[cfg(target_os="linux")]
        {
            if env::var_os("SSL_CERT_FILE").is_none() {
                env::set_var("SSL_CERT_FILE", "/etc/ssl/certs/ca-certificates.crt");
            }
            if env::var_os("SSL_CERT_DIR").is_none() {
                env::set_var("SSL_CERT_DIR", "/etc/ssl/certs");
            }
        }

        let api_url = format!("https://api.github.com/repos/{}/{}/releases/latest", self.repo_owner, self.repo_name);

        print_flush!("Checking target-arch... ");
        println!("{}", self.target);

        println!("Checking current version... v{}", self.current_version);

        print_flush!("Checking latest released version... ");
        let mut resp = reqwest::get(&api_url)?;
        if !resp.status().is_success() { bail!(Error::Update, "api request failed with status: {:?}", resp.status()) }
        let latest: serde_json::Value = resp.json()?;
        let latest_tag = latest["tag_name"].as_str()
            .ok_or_else(|| format_err!(Error::Update, "No tag_name found for latest release"))?
            .trim_left_matches("v");
        println!("v{}", latest_tag);

        if latest_tag.cmp(&self.current_version) != cmp::Ordering::Greater {
            println!("Already up to date! -- v{}", self.current_version);
            return Ok(())
        }

        println!("New release found! v{} --> v{}", self.current_version, latest_tag);

        let latest_assets = latest["assets"].as_array().ok_or_else(|| format_err!(Error::Update, "No release assets found!"))?;
        let target_asset = latest_assets.iter().map(ReleaseAsset::from_asset).collect::<Result<Vec<ReleaseAsset>>>();
        let target_asset = target_asset?.into_iter()
            .filter(|ra| ra.name.contains(&self.target))
            .nth(0)
            .ok_or_else(|| format_err!(Error::Update, "No release asset found for current target: `{}`", self.target))?;

        println!("\n{} release status:", self.bin_name);
        println!("  * Current exe: {:?}", self.bin_install_path);
        println!("  * New exe tarball: {:?}", target_asset.name);
        println!("  * New exe download url: {:?}", target_asset.download_url);
        println!("\nThe new release will be downloaded/extracted and the existing binary will be replaced.");
        prompt_ok("Do you want to continue? [Y/n] ")?;

        let tmp_dir = tempdir::TempDir::new(&format!("__{}-download", self.bin_name))?;
        let tmp_tarball_path = tmp_dir.path().join(&target_asset.name);
        let mut tmp_tarball = fs::File::create(&tmp_tarball_path)?;

        println!("Downloading...");
        download_to_file_with_progress(&target_asset.download_url, &mut tmp_tarball, self.show_progress)?;

        print_flush!("Extracting tarball... ");
        extract_targz(&tmp_tarball_path, &tmp_dir.path())?;
        let new_exe = tmp_dir.path().join(&self.bin_path_in_tarball);
        println!("Done");

        print_flush!("Replacing binary file... ");
        let tmp_file = tmp_dir.path().join(&format!("__{}_backup", self.bin_name));
        replace_exe(&self.bin_install_path, &new_exe, &tmp_file)?;
        println!("Done");

        Ok(())
    }
}
