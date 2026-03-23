use std::time::Instant;
use std::{os::fd::OwnedFd, sync::Mutex};

use cctk::wayland_client::protocol::wl_surface::WlSurface;
use sctk::data_device_manager::data_device::{DataDeviceData, DataDeviceHandler};
use sctk::data_device_manager::data_offer::{DataOfferData, DragOffer, receive_to_fd};
use sctk::reexports::client::Proxy;
use sctk::reexports::client::protocol::wl_data_device::WlDataDevice;
use sctk::reexports::client::protocol::wl_data_device_manager::DndAction as ClientDndAction;
use sctk::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay::input::dnd::{DnDGrab, DndAction as ServerDndAction, Source, SourceMetadata};
use smithay::input::pointer::{Focus, GrabStartData};
use smithay::reexports::wayland_server::Resource;
use smithay::utils::SERIAL_COUNTER;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::selection::data_device::{set_data_device_focus, set_data_device_selection};

use crate::xdg_shell_wrapper::client_state::FocusStatus;
use crate::xdg_shell_wrapper::server_state::ServerPointerFocus;
use crate::xdg_shell_wrapper::shared_state::GlobalState;
use crate::xdg_shell_wrapper::space::WrapperSpace;

fn source_metadata_from_drag_offer(offer: &DragOffer) -> SourceMetadata {
    let mut metadata = SourceMetadata::default();
    if offer.source_actions.contains(ClientDndAction::Copy) {
        metadata.dnd_actions.push(ServerDndAction::Copy);
    }
    if offer.source_actions.contains(ClientDndAction::Move) {
        metadata.dnd_actions.push(ServerDndAction::Move);
    }
    if offer.source_actions.contains(ClientDndAction::Ask) {
        metadata.dnd_actions.push(ServerDndAction::Ask);
    }

    metadata.mime_types =
        offer.inner().data::<DataOfferData>().unwrap().with_mime_types(|m| m.to_vec());
    metadata
}

fn client_dnd_action(action: ServerDndAction) -> ClientDndAction {
    match action {
        ServerDndAction::Copy => ClientDndAction::Copy,
        ServerDndAction::Move => ClientDndAction::Move,
        ServerDndAction::Ask => ClientDndAction::Ask,
        ServerDndAction::None => ClientDndAction::None,
    }
}

#[derive(Debug)]
struct ClientDragOfferSource {
    offer: Mutex<DragOffer>,
    metadata: SourceMetadata,
}

impl smithay::utils::IsAlive for ClientDragOfferSource {
    fn alive(&self) -> bool {
        self.offer.lock().map(|offer| offer.inner().is_alive()).unwrap_or(false)
    }
}

impl Source for ClientDragOfferSource {
    fn metadata(&self) -> Option<SourceMetadata> {
        Some(self.metadata.clone())
    }

    fn choose_action(&self, action: ServerDndAction) {
        if let Ok(offer) = self.offer.lock() {
            let action = client_dnd_action(action);
            offer.set_actions(action, action);
        }
    }

    fn send(&self, mime_type: &str, fd: OwnedFd) {
        if let Ok(offer) = self.offer.lock() {
            receive_to_fd(offer.inner(), mime_type.to_owned(), fd);
        }
    }

    fn drop_performed(&self) {}

    fn cancel(&self) {
        if let Ok(offer) = self.offer.lock() {
            offer.destroy();
        }
    }

    fn finished(&self) {
        if let Ok(offer) = self.offer.lock() {
            offer.finish();
        }
    }
}

impl DataDeviceHandler for GlobalState {
    fn selection(
        &mut self,
        _conn: &sctk::reexports::client::Connection,
        _qh: &sctk::reexports::client::QueueHandle<Self>,
        data_device: &WlDataDevice,
    ) {
        let seat = match self
            .server_state
            .seats
            .iter_mut()
            .find(|sp| sp.client.data_device.inner() == data_device)
        {
            Some(sp) => sp,
            None => return,
        };

        // ignore our own selection offer
        if seat.client.next_selection_offer_is_mine {
            seat.client.next_selection_offer_is_mine = false;
            return;
        }

        let offer = match data_device.data::<DataDeviceData>().unwrap().selection_offer() {
            Some(offer) => offer,
            None => return,
        };
        let wl_offer = offer.inner();

        let mime_types = wl_offer.data::<DataOfferData>().unwrap().with_mime_types(|m| m.to_vec());
        seat.client.selection_offer = Some(offer);
        set_data_device_selection(
            &self.server_state.display_handle,
            &seat.server.seat,
            mime_types,
            (),
        )
    }

    fn enter(
        &mut self,
        _conn: &sctk::reexports::client::Connection,
        _qh: &sctk::reexports::client::QueueHandle<Self>,
        data_device: &WlDataDevice,
        _x: f64,
        _y: f64,
        surface: &WlSurface,
    ) {
        let Some(seat_idx) = self
            .server_state
            .seats
            .iter()
            .position(|sp| sp.client.data_device.inner() == data_device)
        else {
            return;
        };
        let seat_name = self.server_state.seats[seat_idx].name.clone();

        if let Some(f) =
            self.client_state.focused_surface.borrow_mut().iter_mut().find(|f| f.1 == seat_name)
        {
            f.0 = surface.clone();
            f.2 = FocusStatus::Focused;
        }

        let offer = match data_device.data::<DataDeviceData>().unwrap().drag_offer() {
            Some(offer) => offer,
            None => return,
        };

        {
            let mut c_hovered_surface = self.client_state.hovered_surface.borrow_mut();
            if let Some(i) = c_hovered_surface.iter().position(|f| f.1 == seat_name) {
                c_hovered_surface[i].0 = surface.clone();
                c_hovered_surface[i].2 = FocusStatus::Focused;
            } else {
                c_hovered_surface.push((
                    offer.surface.clone(),
                    seat_name.clone(),
                    FocusStatus::Focused,
                ));
            }
        }

        let metadata = source_metadata_from_drag_offer(&offer);
        let (x, y) = (offer.x, offer.y);
        let Some(ptr) =
            self.server_state.seats[seat_idx].client.ptr.as_ref().map(|p| p.pointer().clone())
        else {
            tracing::error!("Missing pointer on seat for dnd enter");
            return;
        };
        let server_focus = self.space.update_pointer(
            (x as i32, y as i32),
            &seat_name,
            offer.surface.clone(),
            &ptr,
        );

        self.server_state.seats[seat_idx].client.dnd_offer = Some(offer.clone());
        // TODO: touch vs pointer start data
        if !self.server_state.seats[seat_idx].client.next_dnd_offer_is_mine {
            let focus = server_focus;
            let server_seat = self.server_state.seats[seat_idx].server.seat.clone();
            let Some(pointer) = server_seat.get_pointer() else {
                tracing::warn!("Missing server pointer on seat for dnd enter");
                return;
            };
            pointer.set_grab(
                self,
                DnDGrab::new_pointer(
                    &self.server_state.display_handle,
                    GrabStartData {
                        focus: focus.map(|f| (f.surface, f.s_pos.to_f64())),
                        button: 0x110, // assume left button for now, maybe there is another way..
                        location: (x, y).into(),
                    },
                    ClientDragOfferSource { offer: Mutex::new(offer), metadata },
                    server_seat,
                ),
                SERIAL_COUNTER.next_serial(),
                Focus::Keep,
            );
        }
    }

    fn leave(
        &mut self,
        conn: &sctk::reexports::client::Connection,
        qh: &sctk::reexports::client::QueueHandle<Self>,
        data_device: &WlDataDevice,
    ) {
        let seat = match self
            .server_state
            .seats
            .iter_mut()
            .find(|sp| sp.client.data_device.inner() == data_device)
        {
            Some(sp) => sp,
            None => return,
        };
        let c_ptr = seat.client.ptr.as_ref().map(|p| p.pointer().clone());
        let s_ptr = seat.server.seat.get_pointer();
        let surface = if let Some(f) =
            self.client_state.focused_surface.borrow_mut().iter_mut().find(|f| f.1 == seat.name)
        {
            f.2 = FocusStatus::LastFocused(Instant::now());
            f.0.clone()
        } else {
            return;
        };

        {
            let mut c_hovered_surface = self.client_state.hovered_surface.borrow_mut();
            if let Some(i) = c_hovered_surface.iter().position(|f| f.0 == surface) {
                c_hovered_surface[i].2 = FocusStatus::LastFocused(Instant::now());
            }
        }

        let duration_since = Instant::now().duration_since(self.start_time).as_millis() as u32;

        let leave_event = PointerEvent {
            surface,
            kind: PointerEventKind::Motion { time: duration_since },
            position: (0.0, 0.0),
        };
        if let Some(s) = s_ptr {
            s.unset_grab(self, SERIAL_COUNTER.next_serial(), 0);
        }

        if let Some(pointer) = c_ptr {
            self.pointer_frame(conn, qh, &pointer, &[leave_event]);
        }
    }

    fn motion(
        &mut self,
        conn: &sctk::reexports::client::Connection,
        qh: &sctk::reexports::client::QueueHandle<Self>,
        data_device: &WlDataDevice,
        _x: f64,
        _y: f64,
    ) {
        // treat it as pointer motion
        let seat = match self
            .server_state
            .seats
            .iter_mut()
            .find(|sp| sp.client.data_device.inner() == data_device)
        {
            Some(sp) => sp,
            None => return,
        };

        let offer = match data_device.data::<DataDeviceData>().unwrap().drag_offer() {
            Some(offer) => offer,
            None => return,
        };
        let Some(ptr) = seat.client.ptr.as_ref().map(|p| p.pointer().clone()) else {
            tracing::error!("Missing pointer on seat for dnd motion");
            return;
        };

        let server_focus = self.space.update_pointer(
            (offer.x as i32, offer.y as i32),
            &seat.name,
            offer.surface.clone(),
            &ptr,
        );

        let client = if let Some(ServerPointerFocus { surface: w, .. }) = server_focus {
            w.wl_surface().and_then(|s| s.client())
        } else {
            None
        };

        set_data_device_focus(&self.server_state.display_handle, &seat.server.seat, client);
        let motion_event = PointerEvent {
            surface: offer.surface.clone(),
            kind: PointerEventKind::Motion { time: offer.time.unwrap_or_default() },
            position: (offer.x, offer.y),
        };

        if let Some(pointer) = seat.client.ptr.as_ref().map(|p| p.pointer().clone()) {
            self.pointer_frame(conn, qh, &pointer, &[motion_event]);
        }
    }

    fn drop_performed(
        &mut self,
        conn: &sctk::reexports::client::Connection,
        qh: &sctk::reexports::client::QueueHandle<Self>,
        data_device: &WlDataDevice,
    ) {
        // treat it as pointer button release
        let seat = match self
            .server_state
            .seats
            .iter_mut()
            .find(|sp| sp.client.data_device.inner() == data_device)
        {
            Some(sp) => sp,
            None => return,
        };

        let offer = match data_device.data::<DataDeviceData>().unwrap().drag_offer() {
            Some(offer) => offer,
            None => return,
        };

        let pointer_event = PointerEvent {
            surface: offer.surface,
            kind: PointerEventKind::Release {
                serial: offer.serial,
                time: offer.time.unwrap_or_default(),
                button: 0x110,
            },
            position: (offer.x, offer.y),
        };
        if let Some(pointer) = seat.client.ptr.as_ref().map(|p| p.pointer().clone()) {
            self.pointer_frame(conn, qh, &pointer, &[pointer_event]);
        }
    }
}
