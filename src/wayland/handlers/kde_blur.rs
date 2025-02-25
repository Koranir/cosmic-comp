use kde_blur::{KdeBlurData, KdeBlurHandler, KdeBlurManagerGlobalData, KdeBlurManagerState};
use smithay::reexports::wayland_server::protocol::wl_region::WlRegion;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{delegate_dispatch, delegate_global_dispatch};
use smithay::wayland::compositor::get_region_attributes;
use wayland_protocols_plasma::blur::server::org_kde_kwin_blur::OrgKdeKwinBlur;
use wayland_protocols_plasma::blur::server::org_kde_kwin_blur_manager::OrgKdeKwinBlurManager;

use crate::shell::element::surface::BlurState;
use crate::state::State;

pub mod kde_blur {
    use smithay::reexports::wayland_server::{
        protocol::{wl_region::WlRegion, wl_surface::WlSurface},
        Dispatch, DisplayHandle, GlobalDispatch,
    };
    use wayland_backend::server::GlobalId;
    use wayland_protocols_plasma::blur::server::{
        org_kde_kwin_blur::OrgKdeKwinBlur, org_kde_kwin_blur_manager::OrgKdeKwinBlurManager,
    };

    pub struct KdeBlurManagerGlobalData;

    pub trait KdeBlurHandler {
        /// A window has requested to be blurred.
        /// By default the whole window should be blurred.
        fn blurred(&mut self, surface: WlSurface);
        /// A window has requested to be unblurred.
        fn unblurred(&mut self, surface: WlSurface);

        /// A window has set its blur bounds.
        /// A no bounds means the whole window should blur.
        fn blur_region(&mut self, surface: WlSurface, region: Option<WlRegion>);
    }

    pub struct KdeBlurData {
        surface: WlSurface,
    }
    impl KdeBlurData {
        pub fn new(surface: WlSurface) -> Self {
            Self { surface }
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
            let _blur_manager = data_init.init(resource, KdeBlurManagerGlobalData);
        }
    }

    impl<D> Dispatch<OrgKdeKwinBlurManager, KdeBlurManagerGlobalData, D> for KdeBlurManagerState
    where
        D: Dispatch<OrgKdeKwinBlur, KdeBlurData, D> + KdeBlurHandler,
    {
        fn request(
            state: &mut D,
            _client: &smithay::reexports::wayland_server::Client,
            _resource: &OrgKdeKwinBlurManager,
            request: <OrgKdeKwinBlurManager as smithay::reexports::wayland_server::Resource>::Request,
            _data: &KdeBlurManagerGlobalData,
            _dhandle: &DisplayHandle,
            data_init: &mut smithay::reexports::wayland_server::DataInit<'_, D>,
        ) {
            match request {
            wayland_protocols_plasma::blur::server::org_kde_kwin_blur_manager::Request::Create { id, surface } => {
                let _kwin_blur = data_init.init(id, KdeBlurData::new(surface.clone()));

                state.blurred(surface);
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
            match request {
                wayland_protocols_plasma::blur::server::org_kde_kwin_blur::Request::Commit => {
                    // TODO: Figure out what this does
                }
                wayland_protocols_plasma::blur::server::org_kde_kwin_blur::Request::SetRegion {
                    region,
                } => state.blur_region(data.surface(), region),
                wayland_protocols_plasma::blur::server::org_kde_kwin_blur::Request::Release => {
                    state.unblurred(data.surface())
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
    fn blurred(&mut self, surface: WlSurface) {
        if let Some(s) = self
            .common
            .shell
            .write()
            .unwrap()
            .element_for_surface(&surface)
        {
            s.active_window().set_blur(BlurState::Blurred);
        }
    }

    fn unblurred(&mut self, surface: WlSurface) {
        if let Some(s) = self
            .common
            .shell
            .write()
            .unwrap()
            .element_for_surface(&surface)
        {
            s.active_window().set_blur(BlurState::Unblurred);
        }
    }

    fn blur_region(&mut self, surface: WlSurface, region: Option<WlRegion>) {
        if let Some(s) = self
            .common
            .shell
            .write()
            .unwrap()
            .element_for_surface(&surface)
        {
            s.active_window().set_blur(match region {
                Some(region) => {
                    let attrs = get_region_attributes(&region);
                    // Kick the can down the road.
                    // TODO: Make blur work for any region:
                    // - Try converting it to a texture, to use it as an alpha mask for blurring?
                    BlurState::PartiallyBlurred(attrs)
                }
                None => BlurState::Blurred,
            });
        }
    }
}
