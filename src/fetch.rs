use crate::errors::*;
use crate::registry::{RegistryIndex, RegistryReq};
use crate::{Dependency, Manifest};
use regex::Regex;
use reqwest::Proxy;
use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};

#[derive(Deserialize)]
struct CrateVersion {
    name: String,
    #[serde(rename = "vers")]
    version: semver::Version,
    yanked: bool,
}

/// Query latest version from a registry index
///
/// The registry argument must be specified for crates
/// from alternative registries.
///
/// The latest version will be returned as a `Dependency`. This will fail, when
///
/// - there is no Internet connection and offline is false.
/// - summaries in registry index with an incorrect format.
/// - a crate with the given name does not exist on the registry.
pub fn get_latest_dependency(
    crate_name: &str,
    flag_allow_prerelease: bool,
    registry: RegistryReq,
) -> Result<Dependency> {
    if env::var("CARGO_IS_TEST").is_ok() {
        // We are in a simulated reality. Nothing is real here.
        // FIXME: Use actual test handling code.
        let new_version = if flag_allow_prerelease {
            format!("{}--PRERELEASE_VERSION_TEST", crate_name)
        } else {
            match crate_name {
                "test_breaking" => "0.2.0".to_string(),
                "test_nonbreaking" => "0.1.1".to_string(),
                other => format!("{}--CURRENT_VERSION_TEST", other),
            }
        };

        return Ok(Dependency::new(crate_name).set_version(&new_version));
    }

    if crate_name.is_empty() {
        return Err(ErrorKind::EmptyCrateName.into());
    }

    let registry_cache = registry.index_url()?.cache_path()?;

    let crate_versions = fuzzy_query_registry_index(crate_name, &registry_cache)?;

    let dep = read_latest_version(&crate_versions, flag_allow_prerelease)?;

    if dep.name != crate_name {
        println!("WARN: Added `{}` instead of `{}`", dep.name, crate_name);
    }

    Ok(dep)
}

// Checks whether a version object is a stable release
fn version_is_stable(version: &CrateVersion) -> bool {
    !version.version.is_prerelease()
}

/// Read latest version from Versions structure
fn read_latest_version(
    versions: &[CrateVersion],
    flag_allow_prerelease: bool,
) -> Result<Dependency> {
    let latest = versions
        .iter()
        .filter(|&v| flag_allow_prerelease || version_is_stable(v))
        .filter(|&v| !v.yanked)
        .max_by_key(|&v| v.version.clone())
        .ok_or(ErrorKind::NoVersionsAvailable)?;

    let name = &latest.name;
    let version = latest.version.to_string();
    Ok(Dependency::new(name).set_version(&version))
}

/// update registry index for given project
pub fn update_registry_index(registry: &RegistryIndex) -> Result<()> {
    let registry_path = registry.cache_path()?;

    let colorchoice = if atty::is(atty::Stream::Stdout) {
        ColorChoice::Auto
    } else {
        ColorChoice::Never
    };
    let mut output = StandardStream::stdout(colorchoice);

    if !registry_path.as_path().exists() {
        // make new bare git repo at registry_path
        output.set_color(ColorSpec::new().set_fg(Some(Color::Green)).set_bold(true))?;
        write!(output, "{:>12}", "Initializing")?;
        output.reset()?;
        writeln!(output, " '{}' index", registry)?;

        let mut opts = git2::RepositoryInitOptions::new();
        opts.bare(true);
        git2::Repository::init_opts(&registry_path, &opts)?;
        return Ok(());
    }

    // use existing git repo at registry_path
    let repo = git2::Repository::open(&registry_path)?;
    output.set_color(ColorSpec::new().set_fg(Some(Color::Green)).set_bold(true))?;
    write!(output, "{:>12}", "Updating")?;
    output.reset()?;
    writeln!(output, " '{}' index", registry)?;

    let refspec = format!(
        "refs/heads/{0}:refs/remotes/origin/{0}",
        get_checkout_name(registry_path)?
    );
    fetch_with_cli(&repo, registry.as_ref().as_str(), &refspec)?;

    Ok(())
}

// https://github.com/rust-lang/cargo/blob/57986eac7157261c33f0123bade7ccd20f15200f/src/cargo/sources/git/utils.rs#L758
fn fetch_with_cli(repo: &git2::Repository, url: &str, refspec: &str) -> Result<()> {
    let cmd = subprocess::Exec::shell("git")
        .arg("fetch")
        .arg("--tags") // fetch all tags
        .arg("--force") // handle force pushes
        .arg("--update-head-ok") // see discussion in rust-lang/cargo#2078
        .arg(url)
        .arg(refspec)
        // If cargo is run by git (for example, the `exec` command in `git
        // rebase`), the GIT_DIR is set by git and will point to the wrong
        // location (this takes precedence over the cwd). Make sure this is
        // unset so git will look at cwd for the repo.
        .env_remove("GIT_DIR")
        // The reset of these may not be necessary, but I'm including them
        // just to be extra paranoid and avoid any issues.
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .cwd(repo.path());

    let _ = cmd.capture().map_err(|e| match e {
        subprocess::PopenError::IoError(io) => ErrorKind::Io(io),
        _ => unreachable!("expected only io error"),
    })?;
    Ok(())
}

#[test]
fn get_latest_stable_version_from_json() {
    let versions: Vec<CrateVersion> = serde_json::from_str(
        r#"[
        {
          "name": "foo",
          "vers": "0.6.0-alpha",
          "yanked": false
        },
        {
          "name": "foo",
          "vers": "0.5.0",
          "yanked": false
        }
      ]"#,
    )
    .expect("crate version is correctly parsed");

    assert_eq!(
        read_latest_version(&versions, false)
            .unwrap()
            .version()
            .unwrap(),
        "0.5.0"
    );
}

#[test]
fn get_latest_unstable_or_stable_version_from_json() {
    let versions: Vec<CrateVersion> = serde_json::from_str(
        r#"[
        {
          "name": "foo",
          "vers": "0.6.0-alpha",
          "yanked": false
        },
        {
          "name": "foo",
          "vers": "0.5.0",
          "yanked": false
        }
      ]"#,
    )
    .expect("crate version is correctly parsed");

    assert_eq!(
        read_latest_version(&versions, true)
            .unwrap()
            .version()
            .unwrap(),
        "0.6.0-alpha"
    );
}

#[test]
fn get_latest_version_from_json_test() {
    let versions: Vec<CrateVersion> = serde_json::from_str(
        r#"[
        {
          "name": "treexml",
          "vers": "0.3.1",
          "yanked": true
        },
        {
          "name": "treexml",
          "vers": "0.3.0",
          "yanked": false
        }
      ]"#,
    )
    .expect("crate version is correctly parsed");

    assert_eq!(
        read_latest_version(&versions, false)
            .unwrap()
            .version()
            .unwrap(),
        "0.3.0"
    );
}

#[test]
fn get_no_latest_version_from_json_when_all_are_yanked() {
    let versions: Vec<CrateVersion> = serde_json::from_str(
        r#"[
        {
          "name": "treexml",
          "vers": "0.3.1",
          "yanked": true
        },
        {
          "name": "treexml",
          "vers": "0.3.0",
          "yanked": true
        }
      ]"#,
    )
    .expect("crate version is correctly parsed");

    assert!(read_latest_version(&versions, false).is_err());
}

/// Gets the checkedout branch name of .cargo/registry/index/github.com-*/.git/refs or
/// .cargo/registry/index/github.com-*/refs for bare git repository
fn get_checkout_name(registry_path: impl AsRef<Path>) -> Result<String> {
    let checkout_dir = registry_path
        .as_ref()
        .join(".git")
        .join("refs/remotes/origin/");
    let bare_checkout_dir = registry_path.as_ref().join("refs/remotes/origin/");

    Ok(checkout_dir
        .read_dir() // .git repo
        .or_else(|_| bare_checkout_dir.read_dir())? // there's no .git, it's bare one
        .next() //Is there always only one branch? (expecting either master og HEAD)
        .ok_or(ErrorKind::MissingRegistraryCheckout(checkout_dir))??
        .file_name()
        .into_string()
        .map_err(|_| ErrorKind::NonUnicodeGitPath)?)
}

/// Fuzzy query crate from registry index
fn fuzzy_query_registry_index(
    crate_name: impl Into<String>,
    registry_path: impl AsRef<Path>,
) -> Result<Vec<CrateVersion>> {
    let crate_name = crate_name.into();
    let remotes = PathBuf::from("refs/remotes/origin/");
    let repo = git2::Repository::open(&registry_path)?;
    let tree = repo
        .find_reference(
            remotes
                .join(get_checkout_name(&registry_path)?)
                .to_str()
                .ok_or(ErrorKind::NonUnicodeGitPath)?,
        )?
        .peel_to_tree()?;

    let mut names = gen_fuzzy_crate_names(crate_name.clone())?;
    if let Some(index) = names.iter().position(|x| *x == crate_name) {
        // ref: https://github.com/killercup/cargo-edit/pull/317#discussion_r307365704
        names.swap(index, 0);
    }

    for the_name in names {
        let file = match tree.get_path(&PathBuf::from(summary_raw_path(&the_name))) {
            Ok(x) => x.to_object(&repo)?.peel_to_blob()?,
            Err(_) => continue,
        };
        let content = String::from_utf8(file.content().to_vec())
            .map_err(|_| ErrorKind::InvalidSummaryJson)?;

        return content
            .lines()
            .map(|line: &str| {
                serde_json::from_str::<CrateVersion>(line)
                    .map_err(|_| ErrorKind::InvalidSummaryJson.into())
            })
            .collect::<Result<Vec<CrateVersion>>>();
    }
    Err(ErrorKind::NoCrate(crate_name).into())
}

fn get_crate_name_from_repository<T>(repo: &str, matcher: &Regex, url_template: T) -> Result<String>
where
    T: Fn(&str, &str) -> String,
{
    matcher
        .captures(repo)
        .ok_or_else(|| "Unable to parse git repo URL".into())
        .and_then(|cap| match (cap.get(1), cap.get(2)) {
            (Some(user), Some(repo)) => {
                let url = url_template(user.as_str(), repo.as_str());
                let data: Result<Manifest> = get_cargo_toml_from_git_url(&url)
                    .and_then(|m| m.parse().chain_err(|| ErrorKind::ParseCargoToml));
                data.and_then(|ref manifest| get_name_from_manifest(manifest))
            }
            _ => Err("Git repo url seems incomplete".into()),
        })
}

/// Query crate name by accessing a github repo Cargo.toml
///
/// The name will be returned as a string. This will fail, when
///
/// - there is no Internet connection,
/// - Cargo.toml is not present in the root of the master branch,
/// - the response from github is an error or in an incorrect format.
pub fn get_crate_name_from_github(repo: &str) -> Result<String> {
    let re =
        Regex::new(r"^https://github.com/([-_0-9a-zA-Z]+)/([-_0-9a-zA-Z]+)(/|.git)?$").unwrap();
    get_crate_name_from_repository(repo, &re, |user, repo| {
        format!(
            "https://raw.githubusercontent.com/{user}/{repo}/master/Cargo.toml",
            user = user,
            repo = repo
        )
    })
}

/// Query crate name by accessing a gitlab repo Cargo.toml
///
/// The name will be returned as a string. This will fail, when
///
/// - there is no Internet connection,
/// - Cargo.toml is not present in the root of the master branch,
/// - the response from gitlab is an error or in an incorrect format.
pub fn get_crate_name_from_gitlab(repo: &str) -> Result<String> {
    let re =
        Regex::new(r"^https://gitlab.com/([-_0-9a-zA-Z]+)/([-_0-9a-zA-Z]+)(/|.git)?$").unwrap();
    get_crate_name_from_repository(repo, &re, |user, repo| {
        format!(
            "https://gitlab.com/{user}/{repo}/raw/master/Cargo.toml",
            user = user,
            repo = repo
        )
    })
}

/// Query crate name by accessing Cargo.toml in a local path
///
/// The name will be returned as a string. This will fail, when
/// Cargo.toml is not present in the root of the path.
pub fn get_crate_name_from_path(path: &str) -> Result<String> {
    let cargo_file = Path::new(path).join("Cargo.toml");
    Manifest::open(&Some(cargo_file))
        .chain_err(|| "Unable to open local Cargo.toml")
        .and_then(|ref manifest| get_name_from_manifest(manifest))
}

fn get_name_from_manifest(manifest: &Manifest) -> Result<String> {
    manifest
        .data
        .as_table()
        .get("package")
        .and_then(|m| m["name"].as_str().map(std::string::ToString::to_string))
        .ok_or_else(|| ErrorKind::ParseCargoToml.into())
}

fn get_cargo_toml_from_git_url(url: &str) -> Result<String> {
    let mut clientb = reqwest::blocking::Client::builder();
    clientb = clientb.timeout(get_default_timeout());
    clientb = clientb.proxy(Proxy::custom(|u| {
        env_proxy::for_url(u).to_url()
    }));
    let client = clientb.build().unwrap();
    
    match client.get(url).send().and_then(|r| r.error_for_status()) {
        Err(e) => {
            Err(format!(
                "HTTP request `{}` failed: {}", url, e
            ).into())
        },
        Ok(res) => {
            res.text().chain_err(|| "Git response not a valid `String`")
        }
    }
}

const fn get_default_timeout() -> Duration {
    Duration::from_secs(10)
}

/// Generate all similar crate names
///
/// Examples:
///
/// | input | output |
/// | ----- | ------ |
/// | cargo | cargo  |
/// | cargo-edit | cargo-edit, cargo_edit |
/// | parking_lot_core | parking_lot_core, parking_lot-core, parking-lot_core, parking-lot-core |
fn gen_fuzzy_crate_names(crate_name: String) -> Result<Vec<String>> {
    const PATTERN: [u8; 2] = [b'-', b'_'];

    let wildcard_indexs = crate_name
        .bytes()
        .enumerate()
        .filter(|(_, item)| PATTERN.contains(item))
        .map(|(index, _)| index)
        .take(10)
        .collect::<Vec<usize>>();
    if wildcard_indexs.is_empty() {
        return Ok(vec![crate_name]);
    }

    let mut result = vec![];
    let mut bytes = crate_name.into_bytes();
    for mask in 0..2u128.pow(wildcard_indexs.len() as u32) {
        for (mask_index, wildcard_index) in wildcard_indexs.iter().enumerate() {
            let mask_value = (mask >> mask_index) & 1 == 1;
            if mask_value {
                bytes[*wildcard_index] = b'-';
            } else {
                bytes[*wildcard_index] = b'_';
            }
        }
        result.push(String::from_utf8(bytes.clone()).unwrap());
    }
    Ok(result)
}

#[test]
fn test_gen_fuzzy_crate_names() {
    fn test_helper(input: &str, expect: &[&str]) {
        let mut actual = gen_fuzzy_crate_names(input.to_string()).unwrap();
        actual.sort();

        let mut expect = expect.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        expect.sort();

        assert_eq!(actual, expect);
    }

    test_helper("", &[""]);
    test_helper("-", &["_", "-"]);
    test_helper("DCjanus", &["DCjanus"]);
    test_helper("DC-janus", &["DC-janus", "DC_janus"]);
    test_helper(
        "DC-_janus",
        &["DC__janus", "DC_-janus", "DC-_janus", "DC--janus"],
    );
}

fn summary_raw_path(crate_name: &str) -> String {
    let crate_name = crate_name.to_ascii_lowercase();
    match crate_name.len() {
        0 => unreachable!("we check that crate_name is not empty here"),
        1 => format!("1/{}", crate_name),
        2 => format!("2/{}", crate_name),
        3 => format!("3/{}/{}", &crate_name[..1], crate_name),
        _ => format!("{}/{}/{}", &crate_name[..2], &crate_name[2..4], crate_name),
    }
}

#[test]
fn test_summary_raw_path() {
    assert_eq!(summary_raw_path("a"), "1/a");
    assert_eq!(summary_raw_path("ab"), "2/ab");
    assert_eq!(summary_raw_path("abc"), "3/a/abc");
    assert_eq!(summary_raw_path("abcd"), "ab/cd/abcd");
    assert_eq!(summary_raw_path("abcdefg"), "ab/cd/abcdefg");
    assert_eq!(summary_raw_path("Inflector"), "in/fl/inflector");
}
