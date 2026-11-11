//! Windows reflink backend: `ReFS` block cloning per file
//! (`FSCTL_DUPLICATE_EXTENTS_TO_FILE`), on a Dev Drive or other `ReFS` volume.

use std::path::Path;

use super::{Backend, ForkError, ForkPaths, ForkStats, TargetSpec, tree};

#[derive(Debug)]
pub(super) struct Refs;

impl Backend for Refs {
    fn fork(&self, paths: &ForkPaths) -> Result<ForkStats, ForkError> {
        tree::clone_tree(&paths.base_target, &paths.agent_target, clone_file)
    }

    fn target_spec(&self, paths: &ForkPaths) -> TargetSpec {
        TargetSpec::Dir(paths.agent_target.clone())
    }

    fn private_bytes(&self, paths: &ForkPaths) -> Result<u64, ForkError> {
        tree::private_bytes_vs_base(&paths.agent_target, &paths.base_target)
    }
}

fn clone_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    crate::sys::clone_file(src, dst)
}
