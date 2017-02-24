use std::fs;
use std::io;
use std::iter::FromIterator;
use std::path::{Path, PathBuf};
use std::collections::HashMap;
use std::collections::hash_map::Values as HashMapValuesIter;

use serde_json;
use xz2::write::XzDecoder;

use super::config::Config;
use super::sdk::SdkInfo;
use super::s3::S3;
use super::{Result, ResultExt};

pub struct MemDbStash<'a> {
    path: &'a Path,
    s3: S3<'a>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RemoteSdk {
    filename: String,
    info: SdkInfo,
    size: u64,
    etag: String,
}

#[derive(Serialize, Deserialize, Default, Debug)]
struct SyncState {
    sdks: HashMap<String, RemoteSdk>,
}

impl RemoteSdk {
    pub fn new(filename: String, info: SdkInfo, etag: String, size: u64) -> RemoteSdk {
        RemoteSdk {
            filename: filename,
            info: info,
            etag: etag,
            size: size,
        }
    }

    pub fn filename(&self) -> &str {
        &self.filename
    }

    pub fn local_filename(&self) -> &str {
        self.filename.trim_right_matches('z')
    }

    pub fn info(&self) -> &SdkInfo {
        &self.info
    }
}

pub type SdksIter<'a> = HashMapValuesIter<'a, String, RemoteSdk>;

impl SyncState {
    pub fn get_sdk(&self, filename: &str) -> Option<&RemoteSdk> {
        self.sdks.get(filename)
    }

    pub fn sdks<'a>(&'a self) -> SdksIter<'a> {
        self.sdks.values()
    }
}

impl<'a> MemDbStash<'a> {
    pub fn new(config: &'a Config) -> Result<MemDbStash<'a>> {
        Ok(MemDbStash {
            path: config.get_symbol_dir()?,
            s3: S3::from_config(config)?,
        })
    }

    fn get_sync_state_filename(&self) -> PathBuf {
        self.path.join("sync.state")
    }

    fn get_local_state(&self) -> Result<SyncState> {
        let filename = self.get_sync_state_filename();
        match fs::File::open(filename) {
            Ok(f) => Ok(serde_json::from_reader(f)
                .chain_err(|| "Parsing error on loading sync state")?),
            Err(err) => {
                if err.kind() == io::ErrorKind::NotFound {
                    Ok(Default::default())
                } else {
                    Err(err).chain_err(|| "Error loading sync state")
                }
            }
        }
    }

    fn save_local_state(&self, new_state: &SyncState) -> Result<()> {
        let filename = self.get_sync_state_filename();
        let mut tmp_filename = filename.clone();
        tmp_filename.set_extension("tempstate");
        {
            let mut f = fs::File::create(&tmp_filename)?;
            serde_json::to_writer(&mut f, new_state)
                .chain_err(|| "Could not update sync state")?;
        }
        fs::rename(&tmp_filename, &filename)?;
        Ok(())
    }

    fn get_remote_state(&self) -> Result<SyncState> {
        let mut sdks = HashMap::new();
        for remote_sdk in self.s3.list_upstream_sdks()? {
            sdks.insert(remote_sdk.local_filename().into(), remote_sdk);
        }
        Ok(SyncState { sdks: sdks })
    }

    fn update_sdk(&self, sdk: &RemoteSdk) -> Result<()> {
        println!("Updating {}", sdk.info());
        let mut src = self.s3.download_sdk(sdk)?;
        let dst = fs::File::create(self.path.join(sdk.local_filename()))?;
        let mut dst = XzDecoder::new(dst);
        io::copy(&mut src, &mut dst)?;
        Ok(())
    }

    fn remove_sdk(&self, sdk: &RemoteSdk) -> Result<()> {
        println!("Deleting {}", sdk.info());
        fs::remove_file(self.path.join(sdk.local_filename()))?;
        Ok(())
    }

    pub fn sync(&self) -> Result<()> {
        let local_state = self.get_local_state()?;
        let remote_state = self.get_remote_state()?;
        let mut to_delete : HashMap<_, _> = HashMap::from_iter(
            local_state.sdks().map(|x| (x.local_filename(), x)));

        for sdk in remote_state.sdks() {
            if let Some(local_sdk) = local_state.get_sdk(sdk.local_filename()) {
                if local_sdk != sdk {
                    self.update_sdk(&sdk)?;
                }
            } else {
                self.update_sdk(&sdk)?;
            }
            to_delete.remove(sdk.local_filename());
        }

        for sdk in to_delete.values() {
            self.remove_sdk(sdk)?;
        }

        println!("Done synching");
        self.save_local_state(&remote_state)?;

        Ok(())
    }
}