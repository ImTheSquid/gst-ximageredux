use std::{sync::{Mutex, atomic::{AtomicBool, Ordering}, Arc, MutexGuard}, time::Duration, ffi::CStr, thread::{JoinHandle, self}, convert::TryInto};

use derivative::Derivative;
use gst::{glib::{self, ffi::{G_LITTLE_ENDIAN, G_BIG_ENDIAN}}, subclass::prelude::{ObjectSubclass, ElementImpl, ObjectImpl, GstObjectImpl, ObjectImplExt, ObjectSubclassExt}, prelude::{ToValue, ElementExtManual, ParamSpecBuilderExt, StaticType, ObjectExt}, FlowError, error_msg};
use gst_app::prelude::BaseSrcExt;
use gst_base::{subclass::{prelude::{BaseSrcImpl, BaseSrcImplExt, PushSrcImpl}, base_src::CreateSuccess}, PushSrc};
use gst_video::ffi::{gst_video_format_from_masks, gst_video_format_to_string};
use once_cell::sync::Lazy;
use anyhow::{Result, bail};
use xcb::{x::{GetGeometry, Drawable, GetImage, self, ImageOrder, ChangeWindowAttributes, Cw, EventMask, QueryPointer, GetProperty}, CookieWithReplyChecked, Connection};
use xcb::x::Event::ConfigureNotify;
use std::convert::TryFrom;
use xcb::x::Event::PropertyNotify;

use gst::{error, trace};

use crate::WindowVisibility;

pub static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ximageredux",
        gst::DebugColorFlags::empty(),
        Some("X11 Window Capture Engine"),
    )
});

type Xid = u32;

#[derive(Derivative)]
#[derivative(Default)]
struct State {
    connection: Option<xcb::Connection>,
    screen_num: Option<i32>,
    xid: Option<Xid>,
    // #[derivative(Default(value="true"))]
    show_cursor: bool,
    #[derivative(Default(value="true"))]
    needs_size_update: bool,
    position: Option<Position>,
    size: Option<Size>,
    frame_duration: Duration,
    last_frame_time: Option<gst::ClockTime>,
    resize_run: Option<Arc<AtomicBool>>,
    resize_handle: Option<JoinHandle<()>>,
    last_frame: Option<gst::Buffer>,
    visibility: WindowVisibility
}

#[derive(Default)]
pub struct XImageRedux {
    state: Arc<Mutex<State>>
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Default)]
struct Size {
    width: u16,
    height: u16
}

#[derive(Debug, PartialEq, Eq)]
struct Position {
    x: i16,
    y: i16
}

impl From<i32> for WindowVisibility {
    fn from(value: i32) -> Self {
        match value {
            1 => Self::Visible,
            2 => Self::Hidden,
            _ => Self::Unknown
        }
    }
}

impl XImageRedux {
    fn get_frame(&self) -> Result<gst::Buffer> {
        self.update_size_if_needed()?;

        let state = self.state.lock().unwrap();
        let (conn, xid) = get_connection(&state)?;

        let cookie = conn.send_request(&GetImage {
            format: x::ImageFormat::ZPixmap, // jpg
            drawable: xcb::x::Drawable::Window(unsafe { xcb::XidNew::new(xid) }),
            x: 0,
            y: 0,
            width: state.size.as_ref().unwrap().width,
            height: state.size.as_ref().unwrap().height,
            plane_mask: u32::MAX,
        });

        let reply = wait_for_reply(conn, cookie)?;

        let mut buf = gst::Buffer::from_slice(reply.data().to_owned());
        let buf = buf.make_mut();
        buf.set_duration(gst::ClockTime::from_mseconds(state.frame_duration.as_millis() as u64));

        Ok(buf.to_owned())
    }

    // Function looks weird to get around mutex issues
    // Returns whether size was updated
    fn update_size_if_needed(&self) -> Result<bool> {
        let should_update = {
            let mut state = self.state.lock().unwrap();

            if state.needs_size_update || state.size.is_none() {
                state.needs_size_update = false;
                true
            } else {
                false
            }
        };

        if should_update {
            let new = self.get_size()?;
            let old_size = self.state.lock().unwrap().size;

            if old_size.is_none() || old_size.unwrap() != new {
                if old_size.is_none() || new.width != old_size.unwrap().width {
                    self.obj().set_property("width", new.width as u32);
                }
                if old_size.is_none() || new.height != old_size.unwrap().height {
                    self.obj().set_property("height", new.height as u32);
                }

                self.obj().emit_by_name::<()>("resize", &[&(new.width as u32), &(new.height as u32)]);
            }

            let _ = self.state.lock().unwrap().size.insert(new);

            let new = self.get_window_visibility()?;
            if new != self.state.lock().unwrap().visibility {
                self.state.lock().unwrap().visibility = new;
                self.obj().set_property("visibility", new);
            }
        }

        Ok(should_update)
    }

    fn get_size(&self) -> Result<Size> {
        let mut state = self.state.lock().unwrap();
        let (conn, xid) = get_connection(&state)?;
        
        let cookie = conn.send_request(&GetGeometry {
            drawable: Drawable::Window(unsafe { xcb::XidNew::new(xid) })
        });

        let reply = wait_for_reply(conn, cookie)?;

        let _ = state.position.insert(Position {
            x: reply.x(),
            y: reply.y()
        });

        Ok(Size {
            width: reply.width(),
            height: reply.height()
        })
    }

    fn get_window_visibility(&self) -> Result<WindowVisibility> {
        let state = self.state.lock().unwrap();
        let (conn, xid) = get_connection(&state)?;

        let cookie = conn.send_request(&GetProperty {
            delete: false,
            window: unsafe { xcb::XidNew::new(xid) },
            property: unsafe { xcb::XidNew::new(320) },
            r#type: x::ATOM_ATOM,
            long_offset: 0,
            long_length: 4
        });

        match conn.wait_for_reply(cookie) {
            Ok(res) => {
                if res.value::<u32>().iter().any(|v| *v == 324) { // Hide
                    Ok(WindowVisibility::Hidden)
                } else { // Show
                    Ok(WindowVisibility::Visible)
                }
            }
            Err(e) => {
                bail!(e);
            }
        }
    }

    fn open_connection(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap();

        let (connection, screen_num) = match xcb::Connection::connect(None) {
            Ok((c, s)) => (c, s),
            Err(e) => bail!("Failed to connect to X11 server: {}", e.to_string())
        };

        let _ = state.connection.insert(connection);
        let _ = state.screen_num.insert(screen_num);

        Ok(())
    }

    unsafe fn get_video_format(&self) -> Result<i32> {
        let state = self.state.lock().unwrap();
        let (conn, xid) = get_connection(&state)?;

        let setup = conn.get_setup();
        let mut endianness = match setup.bitmap_format_bit_order() {
            ImageOrder::MsbFirst => G_BIG_ENDIAN,
            ImageOrder::LsbFirst => G_LITTLE_ENDIAN
        };

        let cookie = conn.send_request(&GetGeometry {
            drawable: Drawable::Window(xcb::XidNew::new(xid))
        });

        let geometry_reply = wait_for_reply(conn, cookie)?;

        let bpp = setup.pixmap_formats().iter().find(|fmt| fmt.depth() == geometry_reply.depth()).unwrap().bits_per_pixel();

        let screen = setup.roots().nth(state.screen_num.unwrap() as usize).unwrap();

        let visual = screen.allowed_depths()
            .flat_map(|depth| depth.visuals().into_iter())
            .find(|vis| vis.visual_id() == screen.root_visual())
            .unwrap();

        // Our caps system handles 24/32bpp RGB as big-endian
        let (red_mask, green_mask, blue_mask) = if (bpp == 24 || bpp == 32) && endianness == G_LITTLE_ENDIAN {
            endianness = G_BIG_ENDIAN;
            let mut set = (visual.red_mask().to_be(), visual.green_mask().to_be(), visual.blue_mask().to_be());

            if bpp == 24 {
                set.0 >>= 8;
                set.1 >>= 8;
                set.2 >>= 8;
            }

            set
        } else {
            (visual.red_mask(), visual.green_mask(), visual.blue_mask())
        };

        let alpha_mask = if bpp == 32 {
            !(red_mask | green_mask | blue_mask)
        } else {
            0
        };

        Ok(gst_video_format_from_masks(geometry_reply.depth().into(), bpp.into(), endianness, red_mask, green_mask, blue_mask, alpha_mask))
    }

    // Returns the relative position of the cursor in the window if it's in the window region
    fn cursor_is_in_bounds(&self) -> Result<Option<Position>> {
        let state = self.state.lock().unwrap();
        let (conn, xid) = get_connection(&state)?;
        let win = unsafe { xcb::XidNew::new(xid) };

        if state.position.is_none() || state.size.is_none() {
            bail!("No position/size set!");
        }

        let cookie = conn.send_request(&QueryPointer {
            window: win
        });

        let reply = wait_for_reply(conn, cookie)?;

        let position = state.position.as_ref().unwrap();
        let size = state.size.as_ref().unwrap();

        let bounds_match = reply.root_x() >= position.x && 
            reply.root_y() >= position.y &&
            reply.root_x() < position.x + i16::try_from(size.width).unwrap() && 
            reply.root_y() < position.y + i16::try_from(size.height).unwrap();

        Ok(if reply.same_screen() && bounds_match {
            Some(Position {
                x: reply.root_x() - position.x,
                y: reply.root_y() - position.y,
            })
        } else { None })
    }
}

fn wait_for_reply<C>(conn: &Connection, cookie: C) -> Result<C::Reply> 
    where C: CookieWithReplyChecked 
    {
        match conn.wait_for_reply(cookie) {
            Ok(reply) => Ok(reply),
            Err(e) => bail!("Failed to wait for X reply: {}", e)
        }
}

fn get_connection<'a>(state: &'a MutexGuard<State>) -> Result<(&'a xcb::Connection, Xid)> {
    let xid = match state.xid {
        Some(xid) => xid,
        None => bail!("XID is not set!"),
    };

    Ok((state.connection.as_ref().unwrap(), xid))
}

#[glib::object_subclass]
impl ObjectSubclass for XImageRedux {
    const NAME: &'static str = "XImageRedux";
    type Type = super::XImageRedux;
    type ParentType = PushSrc;
}

impl PushSrcImpl for XImageRedux {
    fn create(
            &self,
            _buffer: Option<&mut gst::BufferRef>,
        ) -> Result<CreateSuccess, gst::FlowError> {
        // Check if time for next frame
        {
            let mut state = self.state.lock().unwrap();
            if let Some(last_time) = state.last_frame_time {
                if gst::ClockTime::default() - last_time >= gst::ClockTime::from_mseconds(state.frame_duration.as_millis().try_into().unwrap()) {
                    // Time for new frame
                    let _ = state.last_frame_time.insert(gst::ClockTime::default());
                } else if let Some(buf) = state.last_frame.as_ref() {
                    // Not time for new frame yet, use last one if it exists
                    return Ok(CreateSuccess::NewBuffer(buf.clone()));
                }
            }
        }
        
        // Updates size
        match self.update_size_if_needed() {
            Ok(did_update_size) => if did_update_size {
                if let Err(e) = self.negotiate() {
                    error!(CAT, "Failed to renegotiate after resize: {}", e.to_string());
                    return Err(gst::FlowError::Error);
                }
            }
            Err(e) => {
                error!(CAT, "Failed to resize: {}", e.to_string());
                return Err(gst::FlowError::Error);
            }
        }

        // Get a frame
        let frame = match self.get_frame() {
            Ok(f) => f,
            Err(e) => {
                // If failed to get frame, try to use the last one as a temporary measure
                if let Some(buf) = &self.state.lock().unwrap().last_frame {
                    trace!(CAT, "Failed to get frame, but last frame is usable.");
                    return Ok(CreateSuccess::NewBuffer(buf.clone()));
                } else {
                    error!(CAT, "Failed to get frame: {}", e.to_string());
                    return Err(FlowError::Error);
                }
            }
        };

        // Copy cursor in if needed
        if self.state.lock().unwrap().show_cursor {
            match self.cursor_is_in_bounds() {
                Ok(res) => if let Some(_pos) = res {
                    // Trying to get the cursor image causes a crash for some reason so it's disabled for now
                    // Once implemented, set default for show-cursor to true in State struct
                    todo!()
                    
                    // let state = self.state.lock().unwrap();
                    // let (conn, _) = get_connection(&state).unwrap();

                    // let cookie = conn.send_request(&GetCursorImage {});

                    // let reply = conn.wait_for_reply(cookie).unwrap();

                    // println!("Got cursor: {:?}", reply.cursor_image());
                }
                Err(e) => {
                    error!(CAT, "Failed to get cursor position: {}", e.to_string());
                    return Err(gst::FlowError::Error);
                }
            }
        }

        // Set this frame as last
        let _ = self.state.lock().unwrap().last_frame.insert(frame.clone());

        Ok(CreateSuccess::NewBuffer(frame))
    }
}

impl BaseSrcImpl for XImageRedux {
    fn caps(&self, _filter: Option<&gst::Caps>) -> Option<gst::Caps> {
        if self.state.lock().unwrap().connection.is_none() {
            if let Err(e) = self.open_connection() {
                error!(CAT, "Failed to open connection: {}", e);
                return Some(self.obj().pad_template_list().iter().next().unwrap().caps().copy())
            }
        }

        if let Err(e) = self.update_size_if_needed() {
            error!(CAT, "Failed to update size: {}", e.to_string());
            return None;
        }

        let fmt = match unsafe { self.get_video_format() } {
            Ok(fmt) => fmt,
            Err(e) => {
                error!(CAT, "Failed to get video format: {}", e.to_string());
                return None;
            }
        };

        let c_str: &CStr = unsafe { CStr::from_ptr(gst_video_format_to_string(fmt)) };

        let state = self.state.lock().unwrap();
        let size = state.size.as_ref().unwrap();

        Some(gst::Caps::builder("video/x-raw")
            .field("format", &c_str.to_str().unwrap())
            .field("width", &(size.width as i32))
            .field("height", &(size.height as i32))
            .field("framerate", &(gst::FractionRange::new(gst::Fraction::new(0, 1), gst::Fraction::new(i32::MAX, 1))))
            .build())
    }

    fn set_caps(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        if self.state.lock().unwrap().connection.is_none() {
            return Err(gst::LoggableError::new(*CAT, glib::BoolError::new("Not ready!", "imp.rs", "set_caps", 0)));
        }

        let framerate: gst::Fraction = match caps.structure(0).unwrap().value("framerate").unwrap().get() {
            Ok(f) => f,
            Err(e) => return Err(gst::LoggableError::new(*CAT, glib::BoolError::new(format!("Error: {}", e.to_string()), "imp.rs", "set_caps", 0)))
        };

        self.state.lock().unwrap().frame_duration = Duration::from_millis(1000 * framerate.denom() as u64 / framerate.numer() as u64);

        Ok(())
    }

    fn fixate(&self, mut caps: gst::Caps) -> gst::Caps {
        let caps = caps.get_mut().unwrap();

        for i in 0..caps.size() {
            caps.structure_mut(i).unwrap().fixate_field_nearest_fraction("framerate", gst::Fraction::new(25, 1));
        }

        self.parent_fixate(caps.to_owned())
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        if let Err(e) = self.open_connection() {
            return Err(error_msg!(
                gst::ResourceError::Failed,
                [&e.to_string()]
            ))
        }

        let xid = {
            let state_wrap = self.state.lock().unwrap();
            get_connection(&state_wrap).unwrap().1
        };

        let run = Arc::new(AtomicBool::new(true));
        let _  = self.state.lock().unwrap().resize_run.insert(run.clone());

        let state_arc = self.state.clone();

        let _ = self.state.lock().unwrap().resize_handle.insert(thread::spawn(move || {
            let conn = xcb::Connection::connect(None).unwrap().0;

            conn.send_request(&ChangeWindowAttributes {
                window: unsafe { xcb::XidNew::new(xid) },
                value_list: &[Cw::EventMask(EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE)]
            });

            // VERY IMPORTANT
            conn.flush().unwrap();

            let mut last_size = None;

            while run.load(Ordering::SeqCst) {
                match conn.poll_for_event() {
                    Ok(e) => if let Some(ev) = e {
                        if let xcb::Event::X(e) = ev {
                            match e {
                                // Listen for size changes
                                ConfigureNotify(e) => {
                                    let size = Size { width: e.width().into(), height: e.height().into() };

                                    // Don't send window relocation events (size stays the same)
                                    if let Some(last_size) = last_size.as_ref() {
                                        if *last_size == size {
                                            continue;
                                        }
                                    } else {
                                        let _ = last_size.insert(size);
                                    }

                                    state_arc.lock().unwrap().needs_size_update = true;
                                }
                                PropertyNotify(_) => {
                                    state_arc.lock().unwrap().needs_size_update = true;
                                }
                                _ => {}
                            }
                        }
                    },
                    Err(e) => {
                        error!(CAT, "Failed to poll for X event: {e}");
                    }
                }

                thread::sleep(Duration::from_millis(50));
            }
        }));

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        if let Some(run) = self.state.lock().unwrap().resize_run.take() {
            run.store(false, Ordering::SeqCst);
        }

        if let Some(handle) = self.state.lock().unwrap().resize_handle.take() {
            handle.join().unwrap();
        }

        self.state.lock().unwrap().connection.take();

        Ok(())
    }
}

impl ElementImpl for XImageRedux {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "X11 Window Capture Engine",
                "Source/Video",
                "Captures X11 windows",
                "Jack Hogan",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::builder_full()
                .structure(gst::Structure::builder("video/x-raw")
                    .field("framerate", gst::FractionRange::new(gst::Fraction::new(0, 1), gst::Fraction::new(i32::MAX, 1)))
                    .field("width", gst::IntRange::new(0, i32::MAX))
                    .field("height", gst::IntRange::new(0, i32::MAX))
                    .build()
                ).build();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl ObjectImpl for XImageRedux {
    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
            vec! [
                glib::subclass::Signal::builder("resize")
                    // Width, height
                    .param_types([u32::static_type(), u32::static_type()])
                    .build()
            ]
        });

        SIGNALS.as_ref()
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecUInt::builder("xid")
                    .nick("XID")
                    .blurb("XID of window to capture")
                    .build(),
                glib::ParamSpecBoolean::builder("show-cursor")
                    .nick("Show Cursor")
                    .blurb("Whether or not to show the cursor (requires XFixes)")
                    .build(),
                glib::ParamSpecUInt::builder("width")
                    .nick("Width")
                    .blurb("The current window width")
                    .build(),
                glib::ParamSpecUInt::builder("height")
                    .nick("Height")
                    .blurb("The current window height, set by the plugin")
                    .build(),
                glib::ParamSpecEnum::builder::<WindowVisibility>("visibility")
                    .nick("Visibility")
                    .blurb("The current window's visiblity")
                    .build()
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "xid" => self.state.lock().unwrap().xid = Some(value.get::<Xid>().unwrap()),
            "show-cursor" => self.state.lock().unwrap().show_cursor = value.get::<bool>().unwrap(),
            // Doesn't do anything on purpose, just dummy so impls can read values
            "visibility" | "width" | "height" => {},
            _ => unimplemented!()
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "xid" => self.state.lock().unwrap().xid.unwrap_or(0).to_value(),
            "show-cursor" => self.state.lock().unwrap().show_cursor.to_value(),
            "width" => (self.state.lock().unwrap().size.unwrap_or(Size::default()).width as u32).to_value(),
            "height" => (self.state.lock().unwrap().size.unwrap_or(Size::default()).height as u32).to_value(),
            "visibility" => self.state.lock().unwrap().visibility.to_value(),
            _ => unimplemented!()
        }
    }

    fn constructed(&self) {
        self.parent_constructed();
        self.obj().set_live(true);
        self.obj().set_format(gst::Format::Time);
    }
}

impl GstObjectImpl for XImageRedux {}