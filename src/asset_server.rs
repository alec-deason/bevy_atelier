use crate::{
    AssetLoadError, AssetLoadRequestHandler, AssetTypeId, AssetTypeRegistry, LoadRequest,
    HANDLE_ALLOCATOR,
};
use anyhow::Result;
use atelier_importer::BoxedImporter;
pub use atelier_loader::storage::LoadStatus;
use atelier_loader::{
    crossbeam_channel::{unbounded, Receiver, Sender},
    handle::{AssetHandle, GenericHandle, Handle, RefOp, SerdeContext},
    packfile_io::PackfileReader,
    rpc_io::RpcIO,
    storage::{
        AssetLoadOp, DefaultIndirectionResolver, IndirectIdentifier, LoadHandle, LoaderInfoProvider,
    },
    Loader,
};
use bevy_ecs::{Res, Resource, Resources};
use bevy_log::*;
use parking_lot::RwLock;
use std::{
    collections::{HashMap, HashSet},
    env,
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
};
use thiserror::Error;
use type_uuid::{self, TypeUuid};

/// The type used for asset versioning
pub type AssetVersion = usize;

/// Errors that occur while loading assets with an AssetServer
#[derive(Error, Debug)]
pub enum AssetServerError {
    #[error("Asset folder path is not a directory.")]
    AssetFolderNotADirectory(String),
    #[error("Invalid root path")]
    InvalidRootPath,
    #[error("No AssetHandler found for the given extension.")]
    MissingAssetHandler,
    #[error("No AssetLoader found for the given extension.")]
    MissingAssetLoader,
    #[error("No asset registration found for a loaded.")]
    MissingAssetRegistration(AssetTypeId),
    #[error("Encountered an error while loading an asset.")]
    AssetLoadError(#[from] AssetLoadError),
    #[error("Encountered an io error.")]
    Io(#[from] io::Error),
    #[error("Failed to watch asset folder.")]
    AssetWatchError { path: PathBuf },
}

struct LoaderThread {
    // NOTE: these must remain private. the LoaderThread Arc counters are used to determine thread liveness
    // if there is one reference, the loader thread is dead. if there are two references, the loader thread is active
    requests: Arc<RwLock<Vec<LoadRequest>>>,
}

/// Info about a specific asset, such as its path and its current load state
#[derive(Debug)]
pub struct AssetInfo {
    pub load_handle: LoadHandle,
    pub path: PathBuf,
    pub load_state: LoadStatus,
}

enum DaemonState {
    Building(),
}

#[derive(Clone)]
pub enum AssetServerSettings {
    Directory(String),
    Packfile(String),
}
impl AssetServerSettings {
    pub fn default_directory() -> Self {
        AssetServerSettings::Directory("assets".to_string())
    }

    pub fn default_packfile() -> Self {
        AssetServerSettings::Packfile("assets.pack".to_string())
    }
}
impl Default for AssetServerSettings {
    fn default() -> Self {
        if cfg!(feature = "assets-daemon") {
            Self::default_directory()
        } else {
            Self::default_packfile()
        }
    }
}

/// Loads assets from the filesystem on background threads
pub struct AssetServer {
    asset_folders: RwLock<Vec<PathBuf>>,
    loader_threads: RwLock<Vec<LoaderThread>>,
    max_loader_threads: usize,
    asset_handlers: Arc<RwLock<Vec<Box<dyn AssetLoadRequestHandler>>>>,
    // TODO: this is a hack to enable retrieving generic AssetLoader<T>s. there must be a better way!
    loaders: Vec<Resources>,
    pub(crate) loader: Loader,
    ref_op_tx: Sender<RefOp>,
    ref_op_rx: Receiver<RefOp>,
}

impl AssetServer {
    pub fn new(settings: &AssetServerSettings) -> Result<Self> {
        let loader = match settings {
            #[cfg(feature = "assets-daemon")]
            AssetServerSettings::Directory(path) => {
                let path = path.clone();
                std::thread::spawn(move || {
                    atelier_daemon::AssetDaemon::default()
                        .with_importer("png", crate::image::ImageImporter)
                        .with_db_path(".assets_db")
                        .with_address("127.0.0.1:9999".parse().unwrap())
                        .with_asset_dirs(vec![PathBuf::from(path)])
                        .run();
                });
                Loader::new_with_handle_allocator(
                    Box::new(RpcIO::default()),
                    Arc::new(&HANDLE_ALLOCATOR),
                )
            }
            #[cfg(not(feature = "assets-daemon"))]
            AssetServerSettings::Directory(path) => {
                anyhow::bail!("asset-daemon is required in order to load assets from a directory");
            }
            AssetServerSettings::Packfile(path) => {
                println!("packfile: {:?}", path);
                Loader::new_with_handle_allocator(
                    Box::new(PackfileReader::new(std::fs::File::open(path)?).unwrap()),
                    Arc::new(&HANDLE_ALLOCATOR),
                )
            }
        };

        let (tx, rx) = unbounded();
        Ok(AssetServer {
            max_loader_threads: 4,
            asset_folders: Default::default(),
            loader_threads: Default::default(),
            asset_handlers: Default::default(),
            loaders: Default::default(),
            loader,
            ref_op_tx: tx,
            ref_op_rx: rx,
        })
    }

    pub(crate) fn ref_op_tx(&self) -> Sender<RefOp> {
        self.ref_op_tx.clone()
    }

    pub fn get_handle<T: Resource, I: Into<LoadHandle>>(&self, id: I) -> Handle<T> {
        let id: LoadHandle = id.into();
        Handle::<T>::new(self.ref_op_tx(), id).into()
    }

    pub fn get_handle_untyped<I: Into<LoadHandle>>(&self, id: I) -> GenericHandle {
        let id: LoadHandle = id.into();
        GenericHandle::new(self.ref_op_tx(), id)
    }

    pub fn get_handle_path<H: Into<LoadHandle>>(&self, handle: H) -> Option<IndirectIdentifier> {
        unimplemented!("Blocked by https://github.com/amethyst/atelier-assets/issues/77, but why do you want this? Could you please open an issue describing your usecase, thanks.");
    }

    pub fn load<T: Resource, P: ToString>(&self, path: P) -> Handle<T> {
        self.load_untyped(IndirectIdentifier::Path(path.to_string()))
            .into()
    }

    pub fn load_untyped<P: Into<IndirectIdentifier>>(&self, path: P) -> GenericHandle {
        let handle = self.loader.add_ref_indirect(path.into());
        atelier_loader::handle::GenericHandle::new(self.ref_op_tx(), handle)
    }

    pub fn load_folder<P: AsRef<Path>>(
        &self,
        path: P,
    ) -> Result<Vec<GenericHandle>, AssetServerError> {
        unimplemented!("Waiting on the ability to load directories in atelier");
    }

    pub fn process_system(_world: &mut bevy_ecs::World, resources: &mut Resources) {
        let mut asset_server = resources
            .get_mut::<Self>()
            .expect("AssetServer does not exist. Consider adding it as a resource.");
        let asset_type_registry = resources
            .get::<AssetTypeRegistry>()
            .expect("AssetTypeRegistry does not exist. Consider adding it as a resource.");
        let resolver = AssetStorageResolver(&*asset_type_registry, resources);
        asset_server
            .loader
            .process(&resolver, &DefaultIndirectionResolver)
            //TODO: Should this panic?
            .unwrap();
    }
}

struct AssetStorageResolver<'a, 'b>(&'a AssetTypeRegistry, &'b Resources);

impl<'a, 'b> atelier_loader::storage::AssetStorage for AssetStorageResolver<'a, 'b> {
    fn update_asset(
        &self,
        loader_info: &dyn LoaderInfoProvider,
        asset_type_id: &AssetTypeId,
        data: Vec<u8>,
        load_handle: LoadHandle,
        load_op: AssetLoadOp,
        version: u32,
    ) -> Result<(), Box<dyn Error + Send + 'static>> {
        if let Some(registration) = self.0.registrations.get(asset_type_id) {
            let mut result = None;
            let result_ref = &mut result;
            let mut load_op_arg = Some(load_op);
            (registration.get_assets_storage_fn)(
                self.1,
                &mut |storage: &dyn atelier_loader::storage::AssetStorage| {
                    *result_ref = Some(storage.update_asset(
                        loader_info,
                        asset_type_id,
                        //FIXME: This seems like a bad clone
                        data.clone(),
                        load_handle,
                        load_op_arg.take().unwrap(),
                        version,
                    ));
                },
            );
            result.unwrap()
        } else {
            error!(
                "Loaded asset type ID {:?} but it was not registered",
                asset_type_id
            );
            Err(Box::new(AssetServerError::MissingAssetRegistration(
                *asset_type_id,
            )))
        }
    }
    fn commit_asset_version(
        &self,
        asset_type_id: &atelier_core::AssetTypeId,
        load_handle: atelier_loader::LoadHandle,
        version: u32,
    ) {
        if let Some(registration) = self.0.registrations.get(asset_type_id) {
            (registration.get_assets_storage_fn)(
                self.1,
                &mut |storage: &dyn atelier_loader::storage::AssetStorage| {
                    storage.commit_asset_version(asset_type_id, load_handle, version);
                },
            );
        } else {
            error!(
                "Loaded asset type ID {:?} but it was not registered",
                asset_type_id
            );
        }
    }
    fn free(
        &self,
        asset_type_id: &atelier_core::AssetTypeId,
        load_handle: atelier_loader::LoadHandle,
        version: u32,
    ) {
        if let Some(registration) = self.0.registrations.get(asset_type_id) {
            (registration.get_assets_storage_fn)(
                self.1,
                &mut |storage: &dyn atelier_loader::storage::AssetStorage| {
                    storage.free(asset_type_id, load_handle, version);
                },
            );
        } else {
            error!(
                "Loaded asset type ID {:?} but it was not registered",
                asset_type_id
            );
        }
    }
}
