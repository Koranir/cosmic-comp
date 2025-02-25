use kde_blur::{KdeBlurData, KdeBlurHandler, KdeBlurManagerGlobalData, KdeBlurManagerState};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{delegate_dispatch, delegate_global_dispatch};
use wayland_protocols_plasma::blur::server::org_kde_kwin_blur::OrgKdeKwinBlur;
use wayland_protocols_plasma::blur::server::org_kde_kwin_blur_manager::OrgKdeKwinBlurManager;

use crate::shell::element::surface::BlurState;
use crate::state::State;

pub mod kde_blur {
    use std::sync::Mutex;

    use smithay::{
        reexports::wayland_server::{
            protocol::wl_surface::WlSurface, Dispatch, DisplayHandle, GlobalDispatch,
        },
        wayland::compositor::get_region_attributes,
    };
    use wayland_backend::server::GlobalId;
    use wayland_protocols_plasma::blur::server::{
        org_kde_kwin_blur::OrgKdeKwinBlur, org_kde_kwin_blur_manager::OrgKdeKwinBlurManager,
    };

    use crate::shell::element::surface::BlurState;

    pub struct KdeBlurManagerGlobalData;

    pub trait KdeBlurHandler {
        fn blur_state_committed(&mut self, surface: WlSurface, state: BlurState);
    }

    pub struct KdeBlurData {
        surface: WlSurface,
        to_commit: std::sync::Mutex<BlurState>,
    }
    impl KdeBlurData {
        pub fn new(surface: WlSurface, state: BlurState) -> Self {
            Self {
                surface,
                to_commit: Mutex::new(state),
            }
        }

        pub fn surface(&self) -> WlSurface {
            self.surface.clone()
        }
    }

    #[derive(Debug)]
    pub struct KdeBlurManagerState {
        pub id: GlobalId,
    }
    impl KdeBlurManagerState {
        pub fn new<D>(display: &DisplayHandle) -> Self
        where
            D: GlobalDispatch<OrgKdeKwinBlurManager, KdeBlurManagerGlobalData> + 'static,
        {
            let id =
                display.create_global::<D, OrgKdeKwinBlurManager, _>(1, KdeBlurManagerGlobalData);

            Self { id }
        }
    }

    impl<D> GlobalDispatch<OrgKdeKwinBlurManager, KdeBlurManagerGlobalData, D> for KdeBlurManagerState
    where
        D: Dispatch<OrgKdeKwinBlurManager, KdeBlurManagerGlobalData, D>,
    {
        fn bind(
            _state: &mut D,
            _handle: &DisplayHandle,
            _client: &smithay::reexports::wayland_server::Client,
            resource: smithay::reexports::wayland_server::New<OrgKdeKwinBlurManager>,
            _global_data: &KdeBlurManagerGlobalData,
            data_init: &mut smithay::reexports::wayland_server::DataInit<'_, D>,
        ) {
            tracing::debug!("Bound KDE Kwin blur manager interface global");
            let _blur_manager = data_init.init(resource, KdeBlurManagerGlobalData);
        }
    }

    impl<D> Dispatch<OrgKdeKwinBlurManager, KdeBlurManagerGlobalData, D> for KdeBlurManagerState
    where
        D: Dispatch<OrgKdeKwinBlur, KdeBlurData, D> + KdeBlurHandler,
    {
        fn request(
            _state: &mut D,
            _client: &smithay::reexports::wayland_server::Client,
            _resource: &OrgKdeKwinBlurManager,
            request: <OrgKdeKwinBlurManager as smithay::reexports::wayland_server::Resource>::Request,
            _data: &KdeBlurManagerGlobalData,
            _dhandle: &DisplayHandle,
            data_init: &mut smithay::reexports::wayland_server::DataInit<'_, D>,
        ) {
            tracing::debug!("Recieved new request for KDE Kwin blur manager global: {request:?}");
            match request {
            wayland_protocols_plasma::blur::server::org_kde_kwin_blur_manager::Request::Create { id, surface } => {
                let _kwin_blur = data_init.init(id, KdeBlurData::new(surface.clone(), BlurState::Blurred));
            },
            wayland_protocols_plasma::blur::server::org_kde_kwin_blur_manager::Request::Unset { surface: _ } => {},
            _ => unreachable!(),
        }
        }
    }

    impl<D> Dispatch<OrgKdeKwinBlur, KdeBlurData, D> for KdeBlurManagerState
    where
        D: KdeBlurHandler,
    {
        fn request(
            state: &mut D,
            _client: &smithay::reexports::wayland_server::Client,
            _resource: &OrgKdeKwinBlur,
            request: <OrgKdeKwinBlur as smithay::reexports::wayland_server::Resource>::Request,
            data: &KdeBlurData,
            _dhandle: &DisplayHandle,
            _data_init: &mut smithay::reexports::wayland_server::DataInit<'_, D>,
        ) {
            tracing::debug!(
                "Recieved new request for KDE Kwin blur handler for surface {:?}: {request:?}",
                data.surface()
            );
            match request {
                wayland_protocols_plasma::blur::server::org_kde_kwin_blur::Request::Commit => state
                    .blur_state_committed(
                        data.surface.clone(),
                        data.to_commit.lock().unwrap().clone(),
                    ),
                wayland_protocols_plasma::blur::server::org_kde_kwin_blur::Request::SetRegion {
                    region,
                } => {
                    *data.to_commit.lock().unwrap() = match region {
                        Some(s) => BlurState::PartiallyBlurred(get_region_attributes(&s)),
                        None => BlurState::Blurred,
                    }
                }
                wayland_protocols_plasma::blur::server::org_kde_kwin_blur::Request::Release => {
                    *data.to_commit.lock().unwrap() = BlurState::Unblurred;
                }
                _ => unreachable!(),
            }
        }
    }
}

delegate_global_dispatch!(State: [OrgKdeKwinBlurManager: KdeBlurManagerGlobalData] => KdeBlurManagerState);
delegate_dispatch!(State: [OrgKdeKwinBlurManager: KdeBlurManagerGlobalData] => KdeBlurManagerState);
delegate_dispatch!(State: [OrgKdeKwinBlur: KdeBlurData] => KdeBlurManagerState);

impl KdeBlurHandler for State {
    fn blur_state_committed(&mut self, surface: WlSurface, state: BlurState) {
        tracing::debug!("blurring request for surface: {surface:?}");
        if let Some(s) = self
            .common
            .shell
            .write()
            .unwrap()
            .element_for_surface(&surface)
        {
            s.active_window().set_blur(state);
            return;
        }
        if let Some(s) = self
            .common
            .shell
            .write()
            .unwrap()
            .pending_windows
            .iter_mut()
            .find(|s| s.surface == surface)
        {
            s.blur_state = state;
            return;
        }
        tracing::warn!("Blurring request for surface {surface:?}, but surface could not be found");
    }
}
