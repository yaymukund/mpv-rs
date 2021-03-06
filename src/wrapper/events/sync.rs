// Copyright (C) 2016  ParadoxSpiral
//
// This file is part of mpv-rs.
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Lesser General Public
// License as published by the Free Software Foundation; either
// version 2.1 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Lesser General Public License for more details.
//
// You should have received a copy of the GNU Lesser General Public
// License along with this library; if not, write to the Free Software
// Foundation, Inc., 51 Franklin Street, Fifth Floor, Boston, MA  02110-1301  USA

use parking_lot::{Condvar, Mutex};

use crate::{events::*, wrapper::mpv_err, *};

use std::{
    collections::HashMap, ffi::CString, marker::PhantomData, os::raw as ctype, ptr::NonNull,
};

unsafe extern "C" fn event_callback(d: *mut ctype::c_void) {
    (*(d as *mut Condvar)).notify_one();
}

impl Mpv {
    #[inline]
    /// Create a new `Mpv`.
    /// The default settings can be probed by running: `$ mpv --show-profile=libmpv`
    ///
    /// This disables all events.
    pub fn new() -> Result<Mpv> {
        let api_version = unsafe { mpv_sys::mpv_client_api_version() };
        if crate::MPV_CLIENT_API_VERSION != api_version {
            return Err(Error::VersionMismatch {
                linked: crate::MPV_CLIENT_API_VERSION,
                loaded: api_version,
            });
        }

        let ctx = unsafe { mpv_sys::mpv_create() };
        if ctx.is_null() {
            return Err(Error::Null);
        }

        let (ev_iter_notification, ev_to_observe, ev_to_observe_properties, ev_observed) = {
            let ev_iter_notification = Box::new((Mutex::new(false), Condvar::new()));
            unsafe {
                mpv_sys::mpv_set_wakeup_callback(
                    ctx,
                    Some(event_callback),
                    &ev_iter_notification.1 as *const Condvar as *mut Condvar as *mut _,
                );
            }

            (
                ev_iter_notification,
                Mutex::new(Vec::with_capacity(10)),
                Mutex::new(HashMap::with_capacity(10)),
                Mutex::new(Vec::with_capacity(15)),
            )
        };

        for i in 2..24 {
            if let Err(e) = mpv_err((), unsafe { mpv_sys::mpv_request_event(ctx, i, 0) }) {
                unsafe { mpv_sys::mpv_terminate_destroy(ctx) };
                return Err(e);
            }
        }

        mpv_err((), unsafe { mpv_sys::mpv_initialize(ctx) }).or_else(|err| {
            unsafe { mpv_sys::mpv_terminate_destroy(ctx) };
            Err(err)
        })?;

        Ok(Mpv {
            ctx: unsafe { NonNull::new_unchecked(ctx) },
            ev_iter_notification,
            ev_to_observe,
            ev_to_observe_properties,
            ev_observed,
            #[cfg(feature = "protocols")]
            protocols_guard: AtomicBool::new(false),
        })
    }

    #[inline]
    /// Observe given `Event`s via an `EventIter`.
    ///
    /// # Panics
    /// If an event is set to be observed that has been previously set to be observed.
    pub fn observe_events(&self, events: &[Event]) -> Result<EventIter> {
        let mut observe = self.ev_to_observe.lock();
        let mut properties = self.ev_to_observe_properties.lock();

        let len = events.len();
        let mut ids = Vec::with_capacity(len);
        let mut evs = Vec::with_capacity(len);
        let mut props = Vec::with_capacity(len);
        for elem in events {
            if let Event::PropertyChange { ref name, ref data } = *elem {
                if properties.contains_key(name) {
                    panic!("Tried to observe {} twice", name);
                } else {
                    mpv_err((), unsafe {
                        mpv_sys::mpv_request_event(self.ctx.as_ptr(), elem.as_id(), 1)
                    })?;
                    props.push((name, data));
                    ids.push(elem.as_id());
                    evs.push(elem.clone());
                }
            } else {
                for id in &*observe {
                    if elem.as_id() == id.as_id() {
                        panic!("Tried to observe {:?} twice", elem);
                    }
                }

                if let Event::LogMessage { level: lvl, .. } = *elem {
                    let min_level = CString::new(mpv_log_level_as_str(lvl))?;
                    mpv_err((), unsafe {
                        mpv_sys::mpv_request_log_messages(self.ctx.as_ptr(), min_level.as_ptr())
                    })?;
                }

                mpv_err((), unsafe {
                    mpv_sys::mpv_request_event(self.ctx.as_ptr(), elem.as_id(), 1)
                })?;
                ids.push(elem.as_id());
                evs.push(elem.clone());
            }
        }

        let mut props_ins = Vec::with_capacity(len);
        let start_id = properties.len();
        for (i, elem) in props.iter().enumerate() {
            let name = CString::new(&elem.0[..])?;
            let err = mpv_err((), unsafe {
                mpv_sys::mpv_observe_property(
                    self.ctx.as_ptr(),
                    (start_id + i) as _,
                    name.as_ptr(),
                    elem.1.format() as _,
                )
            });
            if err.is_err() {
                // Ignore errors.
                for (_, id) in props_ins {
                    unsafe { mpv_sys::mpv_unobserve_property(self.ctx.as_ptr(), id) };
                }
                return Err(err.unwrap_err());
            }
            props_ins.push((elem.0.clone(), (start_id + i) as _));
        }
        observe.extend(evs.clone());
        properties.extend(props_ins);

        Ok(EventIter {
            ctx: self.ctx,
            first_iteration: true,
            notification: &self.ev_iter_notification,
            all_to_observe: &self.ev_to_observe,
            all_to_observe_properties: &self.ev_to_observe_properties,
            local_to_observe: evs,
            all_observed: &self.ev_observed,
            _does_not_outlive: PhantomData::<&Self>,
        })
    }
}

#[derive(Debug, Clone)]
#[allow(missing_docs)]
/// An event returned by libmpv.
///
/// Equality is implemented as equality between variants, not values.
pub enum Event {
    LogMessage {
        prefix: String,
        level: LogLevel,
        text: String,
    },
    StartFile,
    EndFile {
        reason: EndFileReason,
        error: Option<Error>,
    },
    FileLoaded,
    Idle,
    Tick,
    VideoReconfig,
    AudioReconfig,
    Seek,
    PlaybackRestart,
    PropertyChange {
        name: String,
        data: PropertyData,
    },
}

impl Event {
    /// Create an empty `Event::LogMessage` with given `LogLevel`.
    /// Use this to observe log messages.
    pub fn empty_logmessage(lvl: LogLevel) -> Event {
        Event::LogMessage {
            prefix: "".into(),
            level: lvl,
            text: "".into(),
        }
    }

    /// Create an `Event::PropertyChange` of a `property` with an empty `&'static str`.
    pub fn empty_propertychange(property: String) -> Event {
        Event::PropertyChange {
            name: property,
            data: PropertyData::Flag(false),
        }
    }

    fn as_id(&self) -> EventId {
        match *self {
            Event::LogMessage { .. } => mpv_event_id::LogMessage,
            Event::StartFile => mpv_event_id::StartFile,
            Event::EndFile { .. } => mpv_event_id::EndFile,
            Event::FileLoaded => mpv_event_id::FileLoaded,
            Event::Idle => mpv_event_id::Idle,
            Event::Tick => mpv_event_id::Tick,
            Event::VideoReconfig => mpv_event_id::VideoReconfig,
            Event::AudioReconfig => mpv_event_id::AudioReconfig,
            Event::Seek => mpv_event_id::Seek,
            Event::PlaybackRestart => mpv_event_id::PlaybackRestart,
            Event::PropertyChange { .. } => mpv_event_id::PropertyChange,
        }
    }

    fn structural_eq(&self, rhs: &Event) -> bool {
        match (self, rhs) {
            (&Event::LogMessage { .. }, &Event::LogMessage { .. })
            | (&Event::StartFile, &Event::StartFile)
            | (&Event::EndFile { .. }, &Event::EndFile { .. })
            | (&Event::FileLoaded, &Event::FileLoaded)
            | (&Event::Idle, &Event::Idle)
            | (&Event::Tick, &Event::Tick)
            | (&Event::VideoReconfig, &Event::VideoReconfig)
            | (&Event::AudioReconfig, &Event::AudioReconfig)
            | (&Event::Seek, &Event::Seek)
            | (&Event::PlaybackRestart, &Event::PlaybackRestart)
            | (&Event::PropertyChange { .. }, &Event::PropertyChange { .. }) => true,
            _ => false,
        }
    }

    fn from_raw(raw: &mpv_sys::mpv_event) -> Event {
        assert!(mpv_err((), raw.error).is_ok());
        match raw.event_id {
            mpv_event_id::LogMessage => Event::logmessage_from_mpv_sys(raw.data),
            mpv_event_id::StartFile => Event::StartFile,
            mpv_event_id::EndFile => Event::endfile_from_mpv_sys(raw.data),
            mpv_event_id::FileLoaded => Event::FileLoaded,
            mpv_event_id::Idle => Event::Idle,
            mpv_event_id::Tick => Event::Tick,
            mpv_event_id::VideoReconfig => Event::VideoReconfig,
            mpv_event_id::AudioReconfig => Event::AudioReconfig,
            mpv_event_id::Seek => Event::Seek,
            mpv_event_id::PlaybackRestart => Event::PlaybackRestart,
            mpv_event_id::PropertyChange => Event::property_from_mpv_sys(raw.data),
            _ => unreachable!(),
        }
    }

    fn endfile_from_mpv_sys(raw: *mut ctype::c_void) -> Event {
        assert!(!raw.is_null());
        let raw = unsafe { &mut *(raw as *mut mpv_sys::mpv_event_end_file) };

        assert!(raw.reason.is_positive());
        Event::EndFile {
            reason: raw.reason as _,
            error: {
                let err = mpv_err((), raw.error);
                if err.is_err() {
                    Some(err.unwrap_err())
                } else {
                    None
                }
            },
        }
    }

    fn logmessage_from_mpv_sys(raw: *mut ctype::c_void) -> Event {
        assert!(!raw.is_null());
        let raw = unsafe { &mut *(raw as *mut mpv_sys::mpv_event_log_message) };
        Event::LogMessage {
            prefix: unsafe { mpv_cstr_to_str!(raw.prefix).unwrap().into() },
            level: raw.log_level,
            text: unsafe { mpv_cstr_to_str!(raw.text).unwrap().into() },
        }
    }

    fn property_from_mpv_sys(raw: *mut ctype::c_void) -> Event {
        assert!(!raw.is_null());
        let raw = unsafe { &mut *(raw as *mut mpv_sys::mpv_event_property) };
        Event::PropertyChange {
            name: unsafe { mpv_cstr_to_str!(raw.name).unwrap().into() },
            data: PropertyData::from_raw(raw.format, raw.data),
        }
    }
}

#[derive(Debug, Clone)]
#[allow(missing_docs)]
/// Data that is returned by the `PropertyChange` event.
pub enum PropertyData {
    String(String),
    OsdString(String),
    Flag(bool),
    Int64(i64),
    Double(ctype::c_double),
}

impl PropertyData {
    fn format(&self) -> MpvFormat {
        match *self {
            PropertyData::String(_) => mpv_format::String,
            PropertyData::OsdString(_) => mpv_format::OsdString,
            PropertyData::Flag(_) => mpv_format::Flag,
            PropertyData::Int64(_) => mpv_format::Int64,
            PropertyData::Double(_) => mpv_format::Double,
        }
    }

    fn from_raw(fmt: MpvFormat, ptr: *mut ctype::c_void) -> PropertyData {
        assert!(!ptr.is_null());
        match fmt {
            mpv_format::Flag => PropertyData::Flag(unsafe { *(ptr as *mut i64) } != 0),
            mpv_format::Int64 => PropertyData::Int64(unsafe { *(ptr as *mut _) }),
            mpv_format::Double => PropertyData::Double(unsafe { *(ptr as *mut _) }),
            _ => unreachable!(),
        }
    }
}

fn mpv_log_level_as_str(lvl: LogLevel) -> &'static str {
    match lvl {
        mpv_log_level::None => "no",
        mpv_log_level::Fatal => "fatal",
        mpv_log_level::Error => "error",
        mpv_log_level::Warn => "warn",
        mpv_log_level::Info => "info",
        mpv_log_level::V => "v",
        mpv_log_level::Debug => "debug",
        mpv_log_level::Trace => "trace",
        _ => unreachable!(),
    }
}

/// A blocking `Iterator` over some observed events of an `Mpv` instance.
/// Once the `EventIter` is dropped, it's `Event`s are removed from
/// the "to be observed" queue, therefore new `Event` invocations won't be observed.
pub struct EventIter<'parent> {
    ctx: NonNull<mpv_sys::mpv_handle>,
    first_iteration: bool,
    notification: &'parent (Mutex<bool>, Condvar),
    all_to_observe: &'parent Mutex<Vec<Event>>,
    all_to_observe_properties: &'parent Mutex<HashMap<String, u64>>,
    local_to_observe: Vec<Event>,
    all_observed: &'parent Mutex<Vec<Event>>,
    _does_not_outlive: PhantomData<&'parent Mpv>,
}

impl<'parent> Drop for EventIter<'parent> {
    fn drop(&mut self) {
        let mut all_to_observe = self.all_to_observe.lock();
        let mut all_observed = self.all_observed.lock();
        let mut all_to_observe_properties = self.all_to_observe_properties.lock();

        // Returns true if outer and inner event match, if so, the event is unobserved.
        let mut compare_ev_unobserve = |outer_ev: &Event, inner_ev: &Event| {
            if let Event::PropertyChange { ref name, .. } = *outer_ev {
                let oname = name;
                if let Event::PropertyChange { ref name, .. } = *inner_ev {
                    if oname == name {
                        unsafe {
                            mpv_sys::mpv_unobserve_property(
                                self.ctx.as_ptr(),
                                all_to_observe_properties.remove(oname).unwrap(),
                            );
                        }
                        return true;
                    }
                } else if mpv_event_id::LogMessage == outer_ev.as_id()
                    && mpv_event_id::LogMessage == inner_ev.as_id()
                {
                    let min_level = &*b"none\0";
                    unsafe {
                        mpv_sys::mpv_request_log_messages(
                            self.ctx.as_ptr(),
                            min_level.as_ptr() as _,
                        )
                    };
                    return true;
                }
            } else if outer_ev.structural_eq(inner_ev) {
                unsafe { mpv_sys::mpv_request_event(self.ctx.as_ptr(), inner_ev.as_id(), 0) };
                return true;
            }
            false
        };

        // This removes all events for which compare_ev_unobserve returns true.
        for outer_ev in &self.local_to_observe {
            all_to_observe.retain(|inner_ev| !compare_ev_unobserve(outer_ev, inner_ev));
            all_observed.retain(|inner_ev| !compare_ev_unobserve(outer_ev, inner_ev));
        }
    }
}

impl<'parent> Iterator for EventIter<'parent> {
    type Item = Vec<Event>;

    fn next(&mut self) -> Option<Self::Item> {
        // Loop until some events can be returned
        loop {
            let mut observed = self.all_observed.lock();
            if observed.is_empty() && !self.first_iteration {
                drop(observed);
                self.notification.1.wait(&mut self.notification.0.lock());
                observed = self.all_observed.lock();
            }

            let mut ret_events = Vec::with_capacity(observed.len());
            if observed.is_empty() || self.first_iteration {
                let all_to_observe = self.all_to_observe.lock();
                let mut last = false;
                'events: loop {
                    let event = unsafe { &*mpv_sys::mpv_wait_event(self.ctx.as_ptr(), 0f32 as _) };
                    let ev_id = event.event_id;

                    if ev_id == mpv_event_id::QueueOverflow {
                        // The queue needs to be emptied asap to prevent loss of events
                        // This should happen very rarely, as the queue size is 1k (2016-10-12)
                        break;
                    } else if ev_id == mpv_event_id::None {
                        if last {
                            break;
                        } else {
                            last = true;
                            continue;
                        }
                    }
                    for local_ob_ev in &self.local_to_observe {
                        if ev_id == local_ob_ev.as_id() {
                            ret_events.push(Event::from_raw(event));
                            continue 'events;
                        }
                    }
                    for all_ob_ev in &*all_to_observe {
                        if ev_id == all_ob_ev.as_id() {
                            observed.push(Event::from_raw(event));
                            continue 'events;
                        }
                    }
                }
                if !observed.is_empty() {
                    drop(observed);
                    self.notification.1.notify_all();
                }
            } else {
                // Return true where outer_ev == inner_ev, and push inner_ev to ret_events
                let mut compare_ev = |outer_ev: &Event, inner_ev: &Event| {
                    if let Event::PropertyChange { ref name, .. } = *outer_ev {
                        let oname = name;
                        if let Event::PropertyChange { ref name, .. } = *inner_ev {
                            if oname == name {
                                ret_events.push(inner_ev.clone());
                                return true;
                            }
                        }
                    } else if outer_ev.structural_eq(inner_ev) {
                        ret_events.push(inner_ev.clone());
                        return true;
                    }
                    false
                };
                // Remove events belonging to this EventIter from observed
                for outer_ev in &self.local_to_observe {
                    observed.retain(|inner_ev| !compare_ev(outer_ev, inner_ev));
                }

                if !observed.is_empty() {
                    drop(observed);
                    self.notification.1.notify_all();
                }
            }

            self.first_iteration = false;

            if !ret_events.is_empty() {
                ret_events.shrink_to_fit();
                return Some(ret_events);
            }
        }
    }
}
