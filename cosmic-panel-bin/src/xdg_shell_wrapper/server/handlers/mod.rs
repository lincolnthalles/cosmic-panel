use smithay::wayland::viewporter::ViewportCachedState;
use std::any::Any;
use std::os::fd::OwnedFd;
use std::sync::Mutex;

use itertools::Itertools;
use sctk::data_device_manager::data_offer::receive_to_fd;
use sctk::delegate_subcompositor;
use sctk::reexports::client::protocol::wl_data_device_manager::DndAction as ClientDndAction;
use sctk::shm::multi::MultiPool;
use smithay::backend::renderer::ImportDma;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::input::dnd::{
    DnDGrab, DndAction as ServerDndAction, DndGrabHandler, DndTarget, GrabType, Source,
};
use smithay::input::pointer::{CursorImageAttributes, Focus};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_data_source::WlDataSource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Serial, Transform};
use smithay::wayland::compositor::{SurfaceAttributes, with_states};
use smithay::wayland::dmabuf::{DmabufHandler, ImportNotifier};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::selection::data_device::{
    DataDeviceHandler, WaylandDndGrabHandler, set_data_device_focus,
};
use smithay::wayland::selection::primary_selection::{
    PrimarySelectionHandler, PrimarySelectionState, set_primary_focus,
};
use smithay::wayland::selection::{SelectionHandler, SelectionSource, SelectionTarget};
use smithay::{
    delegate_data_device, delegate_dmabuf, delegate_output, delegate_primary_selection,
    delegate_seat,
};
use tracing::{error, info, trace, warn};

use crate::iced::elements::target::SpaceTarget;
use crate::xdg_shell_wrapper::shared_state::GlobalState;
use crate::xdg_shell_wrapper::space::WrapperSpace;
use crate::xdg_shell_wrapper::util::write_and_attach_buffer;

pub(crate) mod compositor;
pub(crate) mod cursor;
pub(crate) mod fractional;
pub(crate) mod layer;
pub(crate) mod viewporter;
pub(crate) mod xdg_shell;

delegate_subcompositor!(GlobalState);

impl PrimarySelectionHandler for GlobalState {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.server_state.primary_selection_state
    }
}

delegate_primary_selection!(GlobalState);

// Wl Seat
//

impl SeatHandler for GlobalState {
    type KeyboardFocus = SpaceTarget;
    type PointerFocus = SpaceTarget;
    type TouchFocus = SpaceTarget;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.server_state.seat_state
    }

    fn focus_changed(
        &mut self,
        seat: &smithay::input::Seat<Self>,
        focused: Option<&Self::KeyboardFocus>,
    ) {
        let dh = &self.server_state.display_handle;
        let Some(id) = focused.and_then(|s| s.wl_surface()).map(|s| s.id()) else {
            return;
        };
        if let Ok(client) = dh.get_client(id.clone()) {
            set_data_device_focus(dh, seat, Some(client));
            let client2 = dh.get_client(id).unwrap();
            set_primary_focus(dh, seat, Some(client2))
        }
    }

    fn cursor_image(
        &mut self,
        seat: &smithay::input::Seat<Self>,
        image: smithay::input::pointer::CursorImageStatus,
    ) {
        trace!("cursor icon");

        let Some(seat_pair) =
            self.server_state.seats.iter().find(|seat_pair| &seat_pair.server.seat == seat)
        else {
            return;
        };
        let Some(ptr) = seat_pair.client.ptr.as_ref() else {
            return;
        };

        match image {
            smithay::input::pointer::CursorImageStatus::Hidden => {
                let ptr = ptr.pointer();
                ptr.set_cursor(seat_pair.client.last_enter, None, 0, 0);
            },
            smithay::input::pointer::CursorImageStatus::Named(icon) => {
                trace!("Cursor image reset to default");
                if let Err(err) = ptr.set_cursor(&self.client_state.connection, icon) {
                    error!("{}", err);
                }
            },
            smithay::input::pointer::CursorImageStatus::Surface(surface) => {
                trace!("received surface with cursor image");
                let vp = with_states(&surface, |states| {
                    *states.cached_state.get::<ViewportCachedState>().current()
                });

                if let Some((vp, dst)) = self.client_state.cursor_vp.as_ref().zip(vp.dst) {
                    vp.set_destination(dst.w, dst.h);
                }
                if let Some((vp, src)) = self.client_state.cursor_vp.as_ref().zip(vp.src) {
                    vp.set_source(src.loc.x, src.loc.y, src.size.w, src.size.h);
                }

                if self.client_state.multipool.is_none() {
                    self.client_state.multipool = MultiPool::new(&self.client_state.shm_state).ok();
                }
                let multipool = match &mut self.client_state.multipool {
                    Some(m) => m,
                    None => {
                        error!("multipool is missing!");
                        return;
                    },
                };
                let cursor_surface = self.client_state.cursor_surface.get_or_insert_with(|| {
                    self.client_state
                        .compositor_state
                        .create_surface(&self.client_state.queue_handle)
                });

                let last_enter = seat_pair.client.last_enter;

                with_states(&surface, |data| {
                    let mut guard = data.cached_state.get::<SurfaceAttributes>();

                    let surface_attributes = guard.current();
                    let buf = surface_attributes.buffer.as_mut();
                    if let Some(hotspot) = data
                        .data_map
                        .get::<Mutex<CursorImageAttributes>>()
                        .and_then(|m| m.lock().ok())
                        .map(|attr| attr.hotspot)
                    {
                        trace!("Setting cursor {:?}", hotspot);
                        let ptr = ptr.pointer();
                        ptr.set_cursor(last_enter, Some(cursor_surface), hotspot.x, hotspot.y);

                        for ctr in 0..5 {
                            if let Err(e) = write_and_attach_buffer(
                                buf.as_ref().unwrap(),
                                cursor_surface,
                                ctr,
                                multipool,
                            ) {
                                info!("failed to attach buffer to cursor surface: {}", e);
                            } else {
                                break;
                            }
                        }
                    }
                });
            },
        }
    }
}

delegate_seat!(GlobalState);

// Wl Data Device
//

impl DataDeviceHandler for GlobalState {
    fn data_device_state(
        &mut self,
    ) -> &mut smithay::wayland::selection::data_device::DataDeviceState {
        &mut self.server_state.data_device_state
    }
}

fn client_dnd_actions(actions: &[ServerDndAction]) -> ClientDndAction {
    let mut client_actions = ClientDndAction::empty();
    for action in actions {
        match action {
            ServerDndAction::Copy => client_actions |= ClientDndAction::Copy,
            ServerDndAction::Move => client_actions |= ClientDndAction::Move,
            ServerDndAction::Ask => client_actions |= ClientDndAction::Ask,
            ServerDndAction::None => {},
        }
    }

    client_actions
}

fn clone_wl_data_source<S: Source>(source: &S) -> Option<WlDataSource> {
    (source as &dyn Any).downcast_ref::<WlDataSource>().cloned()
}

impl WaylandDndGrabHandler for GlobalState {
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: Serial,
        type_: GrabType,
    ) {
        let Some(seat_idx) = self.server_state.seats.iter().position(|s| s.server.seat == seat)
        else {
            return;
        };
        let seat_name = self.server_state.seats[seat_idx].name.clone();

        let wl_data_source = clone_wl_data_source(&source);

        if let Some(metadata) = source.metadata() {
            self.server_state.seats[seat_idx].client.next_dnd_offer_is_mine = true;
            let dnd_source = self.client_state.data_device_manager.create_drag_and_drop_source(
                &self.client_state.queue_handle,
                metadata.mime_types.iter().map(|m| m.as_str()).collect_vec(),
                client_dnd_actions(&metadata.dnd_actions),
            );
            if let Some(focus) =
                self.client_state.focused_surface.borrow().iter().find(|f| f.1 == seat_name)
            {
                let c_icon_surface = icon.as_ref().map(|_| {
                    self.client_state
                        .compositor_state
                        .create_surface(&self.client_state.queue_handle)
                });
                dnd_source.start_drag(
                    &self.server_state.seats[seat_idx].client.data_device,
                    &focus.0,
                    c_icon_surface.as_ref(),
                    self.server_state.seats[seat_idx].client.get_serial_of_last_seat_event(),
                );
                if let Some(client_surface) = c_icon_surface.as_ref() {
                    client_surface.frame(&self.client_state.queue_handle, client_surface.clone());
                    client_surface.commit();

                    self.server_state.seats[seat_idx].client.dnd_icon = Some((
                        None,
                        client_surface.clone(),
                        OutputDamageTracker::new((32, 32), 1., Transform::Flipped180),
                        false,
                        Some(0),
                    ));
                }
            }
            self.server_state.seats[seat_idx].client.dnd_source = Some(dnd_source);
        }

        self.server_state.seats[seat_idx].server.dnd_source = wl_data_source;
        self.server_state.seats[seat_idx].server.dnd_icon = icon.clone();
        let server_seat = self.server_state.seats[seat_idx].server.seat.clone();

        match type_ {
            GrabType::Pointer => {
                let Some(pointer) = server_seat.get_pointer() else {
                    warn!("Missing server pointer for client-initiated drag-and-drop");
                    return;
                };
                let Some(start_data) = pointer.grab_start_data() else {
                    warn!("Missing pointer grab start data for client-initiated drag-and-drop");
                    return;
                };
                pointer.set_grab(
                    self,
                    DnDGrab::new_pointer(
                        &self.server_state.display_handle,
                        start_data,
                        source,
                        server_seat,
                    ),
                    serial,
                    Focus::Keep,
                );
            },
            GrabType::Touch => {
                let Some(touch) = server_seat.get_touch() else {
                    warn!("Missing server touch handle for client-initiated drag-and-drop");
                    return;
                };
                let Some(start_data) = touch.grab_start_data() else {
                    warn!("Missing touch grab start data for client-initiated drag-and-drop");
                    return;
                };
                touch.set_grab(
                    self,
                    DnDGrab::new_touch(
                        &self.server_state.display_handle,
                        start_data,
                        source,
                        server_seat,
                    ),
                    serial,
                );
            },
        }
    }
}

impl DndGrabHandler for GlobalState {
    fn dropped(
        &mut self,
        _target: Option<DndTarget<'_, Self>>,
        _validated: bool,
        seat: Seat<Self>,
        _location: Point<f64, Logical>,
    ) {
        let seat = match self.server_state.seats.iter_mut().find(|s| s.server.seat == seat) {
            Some(s) => s,
            None => return,
        };

        seat.server.dnd_source = None;
        seat.server.dnd_icon = None;
        seat.client.dnd_offer = None;
        seat.client.dnd_icon = None;
        seat.client.dnd_source = None;
    }
}

delegate_data_device!(GlobalState);

// Wl Output
//

delegate_output!(GlobalState);

impl OutputHandler for GlobalState {}
// Dmabuf
//
impl DmabufHandler for GlobalState {
    fn dmabuf_state(&mut self) -> &mut smithay::wayland::dmabuf::DmabufState {
        &mut self.server_state.dmabuf_state.0
    }

    fn dmabuf_imported(
        &mut self,
        _global: &smithay::wayland::dmabuf::DmabufGlobal,
        dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        _: ImportNotifier,
    ) {
        if let Some(Err(err)) =
            self.space.renderer().map(|renderer| renderer.import_dmabuf(&dmabuf, None))
        {
            error!("Failed to import dmabuf: {}", err);
            self.server_state.dmabuf_import_failures += 1;
            if self.server_state.dmabuf_import_failures >= 8
                && let Some(global) = self.server_state.dmabuf_state.1.take()
            {
                warn!(
                    "Disabling dmabuf global after repeated import failures ({} failures)",
                    self.server_state.dmabuf_import_failures
                );
                self.server_state
                    .dmabuf_state
                    .0
                    .disable_global::<GlobalState>(&self.server_state.display_handle, &global);
            }
            return;
        }
        self.server_state.dmabuf_import_failures = 0;
    }
}

impl SelectionHandler for GlobalState {
    type SelectionUserData = ();

    fn new_selection(
        &mut self,
        _target: SelectionTarget,
        source: Option<SelectionSource>,
        seat: Seat<GlobalState>,
    ) {
        let seat = match self.server_state.seats.iter_mut().find(|s| s.server.seat == seat) {
            Some(s) => s,
            None => return,
        };

        let serial = seat.client.get_serial_of_last_seat_event();

        if let Some(source) = source {
            seat.client.next_selection_offer_is_mine = true;
            let mime_types = source.mime_types();
            let copy_paste_source = self
                .client_state
                .data_device_manager
                .create_copy_paste_source(&self.client_state.queue_handle, mime_types);
            copy_paste_source.set_selection(&seat.client.data_device, serial);
            seat.client.copy_paste_source = Some(copy_paste_source);
            seat.server.selection_source = Some(source);
        } else {
            seat.client.data_device.unset_selection(serial)
        }
    }

    fn send_selection(
        &mut self,
        _target: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
        seat: Seat<Self>,
        _: &Self::SelectionUserData,
    ) {
        let seat = match self.server_state.seats.iter().find(|s| s.server.seat == seat) {
            Some(s) => s,
            None => return,
        };
        if let Some(offer) = seat.client.selection_offer.as_ref() {
            receive_to_fd(offer.inner(), mime_type, fd)
        }
    }
}

delegate_dmabuf!(GlobalState);
