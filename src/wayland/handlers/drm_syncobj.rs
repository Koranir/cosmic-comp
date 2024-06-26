// SPDX-License-Identifier: GPL-3.0-only

use crate::state::{BackendData, State};
use smithay::{
    backend::drm::DrmDeviceFd, delegate_drm_syncobj, wayland::drm_syncobj::DrmSyncobjHandler,
};

impl DrmSyncobjHandler for State {
    fn import_device(&self) -> &DrmDeviceFd {
        let kms = match &self.backend {
            BackendData::Kms(kms) => kms,
            _ => unreachable!(),
        };
        // TODO only expose protocol if we have node
        kms.drm_devices[&kms
            .primary_node
            .unwrap() // XXX
            .node_with_type(smithay::backend::drm::NodeType::Primary)
            .unwrap()
            .unwrap()]
            .drm
            .device_fd()
    }
}

delegate_drm_syncobj!(State);
