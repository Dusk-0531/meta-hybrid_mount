// Copyright (C) 2026 YuzakiKokuban <heibanbaize@gmail.com>
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    collections::BTreeSet,
    env, fs,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

use anyhow::Result;
#[cfg(any(target_os = "linux", target_os = "android"))]
use rustix::mount::{UnmountFlags, unmount as umount};

#[cfg(feature = "kasumi")]
use crate::core::kasumi_coordinator::KasumiCoordinator;
#[cfg(feature = "kasumi")]
use crate::core::recovery::ModuleStageFailure;
#[cfg(any(target_os = "linux", target_os = "android"))]
use crate::sys::mount::is_mounted;
use crate::{
    conf::config::Config,
    core::{
        backend_capabilities::BackendCapabilities,
        inventory::{self},
        ops::{
            executor::{self},
            plan::MountPlan,
            prepare,
        },
        runtime_finalization,
        storage::StorageHandle,
    },
    defs,
};

pub struct Init;

pub struct StorageReady {
    pub handle: StorageHandle,
}

pub struct Planned {
    pub handle: StorageHandle,
    pub modules: Vec<inventory::Module>,
    pub plan: MountPlan,
}

pub struct Executed {
    pub handle: StorageHandle,
    pub result: executor::ExecutionResult,
}

pub struct MountController<S> {
    config: Config,
    backend_capabilities: BackendCapabilities,
    state: S,
    tempdir: PathBuf,
}

impl MountController<Init> {
    pub fn new<P>(config: Config, tempdir: P) -> Self
    where
        P: AsRef<Path>,
    {
        Self {
            backend_capabilities: BackendCapabilities::detect(&config),
            config,
            state: Init,
            tempdir: tempdir.as_ref().to_path_buf(),
        }
    }

    pub fn init_storage(self, mnt_base: &Path) -> Result<MountController<StorageReady>> {
        crate::scoped_log!(
            info,
            "controller:init_storage",
            "start: mount_base={}",
            mnt_base.display()
        );
        #[cfg(feature = "control-plane")]
        let force_ext4 = matches!(
            self.config.overlay_mode,
            crate::conf::config::OverlayMode::Ext4
        );
        #[cfg(not(feature = "control-plane"))]
        let force_ext4 = true;
        let handle = crate::core::storage::setup(
            mnt_base,
            &self.config.moduledir,
            force_ext4,
            &self.config.mountsource,
            self.config.disable_umount,
        )?;

        crate::scoped_log!(
            info,
            "controller:init_storage",
            "complete: mode={}, mount_point={}",
            handle.mode().as_str(),
            handle.mount_point().display()
        );

        Ok(MountController {
            config: self.config,
            backend_capabilities: self.backend_capabilities,
            state: StorageReady { handle },
            tempdir: self.tempdir,
        })
    }
}

impl MountController<StorageReady> {
    pub fn scan_and_prepare_plan(self) -> Result<MountController<Planned>> {
        crate::scoped_log!(
            info,
            "controller:scan_and_prepare_plan",
            "scan start: moduledir={}",
            self.config.moduledir.display()
        );
        let modules = inventory::scan(&self.config.moduledir, &self.config)?;

        crate::scoped_log!(
            info,
            "controller:scan_and_prepare_plan",
            "scan complete: modules={}",
            modules.len()
        );

        crate::scoped_log!(info, "controller:scan_and_prepare_plan", "prepare start");
        let plan = prepare::prepare_mount_plan(
            &self.config,
            &modules,
            self.state.handle.mount_point(),
            &self.backend_capabilities,
        )?;

        crate::scoped_log!(
            info,
            "controller:scan_and_prepare_plan",
            "prepare complete: overlay_ops={}, overlay_modules={}, magic_modules={}, kasumi_modules={}, kasumi_rule_compile=deferred",
            plan.overlay_ops.len(),
            plan.overlay_module_ids.len(),
            plan.magic_module_ids.len(),
            plan.kasumi_module_ids.len()
        );

        #[cfg(feature = "kasumi")]
        {
            let kasumi = KasumiCoordinator::new(&self.config);
            kasumi
                .prepare_mirror_storage(
                    &self.backend_capabilities,
                    &modules,
                    &plan,
                    self.state.handle.mount_point(),
                )
                .map_err(|err| {
                    ModuleStageFailure::sync(
                        plan.kasumi_module_ids.clone(),
                        anyhow::anyhow!("Failed to prepare Kasumi mirror storage: {:#}", err),
                    )
                })?;
        }

        Ok(MountController {
            config: self.config,
            backend_capabilities: self.backend_capabilities,
            state: Planned {
                handle: self.state.handle,
                modules,
                plan,
            },
            tempdir: self.tempdir,
        })
    }
}

impl MountController<Planned> {
    pub fn execute(mut self) -> Result<MountController<Executed>> {
        crate::scoped_log!(info, "controller:execute", "start");
        sync_skip_mount_markers(&self.config, &self.state.plan)?;
        let result = executor::Executor::execute(
            &mut self.state.plan,
            &self.state.modules,
            &self.config,
            self.tempdir.clone(),
        )?;

        crate::scoped_log!(
            info,
            "controller:execute",
            "complete: overlay_mounted={}, magic_mounted={}, kasumi_mounted={}",
            result.overlay_module_ids.len(),
            result.magic_module_ids.len(),
            result.kasumi_module_ids.len()
        );

        Ok(MountController {
            config: self.config,
            backend_capabilities: self.backend_capabilities,
            state: Executed {
                handle: self.state.handle,
                result,
            },
            tempdir: self.tempdir,
        })
    }
}

fn sync_skip_mount_markers(config: &Config, plan: &MountPlan) -> Result<()> {
    let should_manage = should_manage_skip_mount_markers();
    if !should_manage {
        clear_skipped_modules_log();
    }

    let mut module_ids = BTreeSet::new();
    module_ids.extend(plan.overlay_module_ids.iter().cloned());
    module_ids.extend(plan.magic_module_ids.iter().cloned());
    module_ids.extend(plan.kasumi_module_ids.iter().cloned());

    for module_id in module_ids {
        let module_dir = config.moduledir.join(&module_id);
        if !module_dir.is_dir() {
            continue;
        }
        let skip_mount_path = module_dir.join(defs::SKIP_MOUNT_FILE_NAME);
        if should_manage {
            if !skip_mount_path.exists() {
                fs::write(&skip_mount_path, b"")?;
                append_skipped_module(&module_id)?;
                crate::scoped_log!(
                    debug,
                    "controller:skip_mount",
                    "create: module={}, path={}",
                    module_id,
                    skip_mount_path.display()
                );
            }
        } else if let Err(err) = fs::remove_file(&skip_mount_path)
            && err.kind() != ErrorKind::NotFound
        {
            return Err(err.into());
        }
    }

    Ok(())
}

fn should_manage_skip_mount_markers() -> bool {
    !(is_apatch_litemode() || is_metamodule())
}

fn is_metamodule() -> bool {
    Path::new(defs::HYBRID_MOUNT_MODULE_DIR)
        .join("metamount.sh")
        .exists()
}

fn is_apatch_litemode() -> bool {
    env::var("APATCH_BIND_MOUNT")
        .ok()
        .is_some_and(|value| value == "true")
        && Path::new("/data/adb/.litemode_enable").exists()
}

fn append_skipped_module(module_id: &str) -> Result<()> {
    let log_path = Path::new(defs::SKIPPED_MODULES_FILE);
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    writeln!(file, "{module_id}")?;
    Ok(())
}

fn clear_skipped_modules_log() {
    if let Err(err) = fs::remove_file(defs::SKIPPED_MODULES_FILE)
        && err.kind() != ErrorKind::NotFound
    {
        crate::scoped_log!(
            warn,
            "controller:skip_mount",
            "failed to clear skipped modules log: path={}, error={:#}",
            defs::SKIPPED_MODULES_FILE,
            err
        );
    }
}

impl MountController<Executed> {
    pub fn finalize(self) -> Result<()> {
        crate::scoped_log!(info, "controller:finalize", "start");
        runtime_finalization::finalize(
            &self.config,
            self.state.handle.mode(),
            self.state.handle.mount_point(),
            &self.state.result,
        )?;

        clean_up(
            &self.tempdir,
            &self.config.kasumi.mirror_path,
            self.state.handle.mode(),
            self.config.disable_umount,
        )?;

        crate::scoped_log!(info, "controller:finalize", "complete");

        Ok(())
    }
}

fn clean_up(
    tempdir: &Path,
    kasumi_mirror_path: &Path,
    storage_mode: crate::core::storage::StorageMode,
    disable_umount: bool,
) -> Result<()> {
    if disable_umount {
        crate::scoped_log!(
            debug,
            "controller:finalize",
            "cleanup skipped: path={}, reason=disable_umount",
            tempdir.display()
        );
        return Ok(());
    }

    if !tempdir.starts_with("/mnt") {
        crate::scoped_log!(
            debug,
            "controller:finalize",
            "cleanup skipped: path={}, reason=outside_mnt",
            tempdir.display()
        );
        return Ok(());
    }

    clean_up_path(tempdir, kasumi_mirror_path, storage_mode)
}

fn clean_up_path(
    tempdir: &Path,
    kasumi_mirror_path: &Path,
    storage_mode: crate::core::storage::StorageMode,
) -> Result<()> {
    if tempdir == kasumi_mirror_path {
        crate::scoped_log!(
            info,
            "controller:finalize",
            "cleanup skipped: path={}, reason=kasumi_mirror",
            tempdir.display()
        );
        return Ok(());
    }

    if kasumi_mirror_path.starts_with(tempdir) {
        let Some(preserved_child) = kasumi_mirror_path
            .strip_prefix(tempdir)
            .ok()
            .and_then(|relative| relative.components().next())
            .map(|component| component.as_os_str().to_owned())
        else {
            return Ok(());
        };

        crate::scoped_log!(
            info,
            "controller:finalize",
            "cleanup partial: path={}, preserve={}",
            tempdir.display(),
            kasumi_mirror_path.display()
        );

        let entries = match fs::read_dir(tempdir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err.into()),
        };

        for entry in entries {
            let entry = entry?;
            if entry.file_name() == preserved_child {
                continue;
            }
            remove_path(&entry.path())?;
        }

        return Ok(());
    }

    crate::scoped_log!(
        info,
        "controller:finalize",
        "cleanup: remove={}",
        tempdir.display()
    );
    detach_tempdir_mount(tempdir)?;
    remove_path(tempdir)?;

    crate::core::storage::cleanup_artifacts(storage_mode)?;
    Ok(())
}

fn detach_tempdir_mount(tempdir: &Path) -> Result<()> {
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let _ = tempdir;
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        if !is_mounted(tempdir) {
            return Ok(());
        }

        crate::scoped_log!(
            info,
            "controller:finalize",
            "cleanup umount: path={}",
            tempdir.display()
        );
        if let Err(err) = umount(tempdir, UnmountFlags::DETACH) {
            crate::scoped_log!(
                warn,
                "controller:finalize",
                "cleanup umount failed: path={}, error={:#}",
                tempdir.display(),
                err
            );
            return Err(err.into());
        }
        crate::scoped_log!(
            info,
            "controller:finalize",
            "cleanup umount complete: path={}",
            tempdir.display()
        );
        Ok(())
    }
}

fn remove_path(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };

    if metadata.file_type().is_dir() {
        if let Err(err) = fs::remove_dir_all(path) {
            if err.raw_os_error() == Some(libc::EBUSY) {
                crate::scoped_log!(
                    warn,
                    "controller:finalize",
                    "cleanup skipped: path={}, reason=resource_busy",
                    path.display()
                );
                return Ok(());
            }
            return Err(err.into());
        }
    } else if let Err(err) = fs::remove_file(path) {
        if err.raw_os_error() == Some(libc::EBUSY) {
            crate::scoped_log!(
                warn,
                "controller:finalize",
                "cleanup skipped: path={}, reason=resource_busy",
                path.display()
            );
            return Ok(());
        }
        return Err(err.into());
    }

    Ok(())
}
