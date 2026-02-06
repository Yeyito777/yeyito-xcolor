use anyhow::{anyhow, Result};
use std::thread;
use std::time::{Duration, Instant};

use x11::{keysym, xlib};
use xcb::base as xbase;
use xcb::base::Connection;
use xcb::xproto;

use crate::color::{self, ARGB};
use crate::draw::draw_magnifying_glass;
use crate::pixel::PixelSquare;
use crate::util::EnsureOdd;

// Left mouse button
const SELECTION_BUTTON: xproto::Button = 1;
const GRAB_MASK: u16 = (xproto::EVENT_MASK_BUTTON_PRESS | xproto::EVENT_MASK_POINTER_MOTION) as u16;

// Creates an invisible 1x1 cursor using pure XCB (no Xlib). Used to hide the real cursor
// during the pointer grab while the magnifier window is shown.
fn create_blank_cursor(conn: &Connection, screen: &xproto::Screen) -> Result<u32> {
    let cursor_id = conn.generate_id();
    let pixmap_id = conn.generate_id();
    let gc_id = conn.generate_id();
    let root = screen.root();

    // Create a 1x1 depth-1 pixmap (bitmap)
    xproto::create_pixmap(conn, 1, pixmap_id, root, 1, 1);

    // Clear the pixmap to 0 so the mask is fully transparent
    xproto::create_gc(conn, gc_id, pixmap_id, &[(xproto::GC_FOREGROUND, 0)]);
    xproto::poly_fill_rectangle(conn, pixmap_id, gc_id, &[xproto::Rectangle::new(0, 0, 1, 1)]);

    // Create invisible cursor: source and mask are both all-zero, so no pixels are visible
    xproto::create_cursor(conn, cursor_id, pixmap_id, pixmap_id, 0, 0, 0, 0, 0, 0, 0, 0);

    xproto::free_gc(conn, gc_id);
    xproto::free_pixmap(conn, pixmap_id);
    conn.flush();

    Ok(cursor_id)
}

const KEYBOARD_GRAB_TIMEOUT: Duration = Duration::from_secs(1);
const KEYBOARD_GRAB_RETRY_DELAY: Duration = Duration::from_millis(5);

// Exclusively grabs the pointer so we get all its events
fn grab_pointer(conn: &Connection, root: u32, cursor: u32) -> Result<()> {
    let reply = xproto::grab_pointer(
        conn,
        false,
        root,
        GRAB_MASK,
        xproto::GRAB_MODE_ASYNC as u8,
        xproto::GRAB_MODE_ASYNC as u8,
        xbase::NONE,
        cursor,
        xbase::CURRENT_TIME,
    )
    .get_reply()?;

    if reply.status() != xproto::GRAB_STATUS_SUCCESS as u8 {
        return Err(anyhow!("Could not grab pointer"));
    }

    Ok(())
}

// Aggressively tries to grab the keyboard so we can listen for ESC to exit
fn grab_keyboard_with_retry(conn: &Connection, root: u32) -> Result<()> {
    let start = Instant::now();

    loop {
        let reply = xproto::grab_keyboard(
            conn,
            false,
            root,
            xbase::CURRENT_TIME,
            xproto::GRAB_MODE_ASYNC as u8,
            xproto::GRAB_MODE_ASYNC as u8,
        )
        .get_reply()?;

        if reply.status() == xproto::GRAB_STATUS_SUCCESS as u8 {
            return Ok(());
        }

        if start.elapsed() >= KEYBOARD_GRAB_TIMEOUT {
            break;
        }

        thread::sleep(KEYBOARD_GRAB_RETRY_DELAY);
    }

    Err(anyhow!(
        "Could not grab keyboard for escape detection after repeated attempts"
    ))
}

fn escape_keycode(conn: &Connection) -> Result<u8> {
    let keycode =
        unsafe { xlib::XKeysymToKeycode(conn.get_raw_dpy(), keysym::XK_Escape as xlib::KeySym) };

    if keycode == 0 {
        Err(anyhow!("Could not resolve the Escape keycode"))
    } else {
        Ok(keycode as u8)
    }
}

fn return_keycode(conn: &Connection) -> Result<u8> {
    let keycode =
        unsafe { xlib::XKeysymToKeycode(conn.get_raw_dpy(), keysym::XK_Return as xlib::KeySym) };

    if keycode == 0 {
        Err(anyhow!("Could not resolve the Return keycode"))
    } else {
        Ok(keycode as u8)
    }
}

// Finds a 32-bit TrueColor visual for creating ARGB windows with transparency
fn find_argb_visual(screen: &xproto::Screen) -> Option<u32> {
    for depth in screen.allowed_depths() {
        if depth.depth() == 32 {
            for visual in depth.visuals() {
                if visual.class() == xproto::VISUAL_CLASS_TRUE_COLOR as u8 {
                    return Some(visual.visual_id());
                }
            }
        }
    }
    None
}

// Extracts a square region around a point from a cached full-screen screenshot.
// Handles screen-edge clamping the same way get_window_rect_around_pointer did.
fn get_rect_from_cache(
    cache: &[ARGB],
    cache_width: u16,
    cache_height: u16,
    (pointer_x, pointer_y): (i16, i16),
    preview_width: u32,
    scale: u32,
) -> (u16, Vec<ARGB>) {
    let root_width = cache_width as isize;
    let root_height = cache_height as isize;

    let size = ((preview_width / scale) as isize).ensure_odd();

    let mut x = (pointer_x as isize) - (size / 2);
    let mut y = (pointer_y as isize) - (size / 2);
    let x_offset = if x < 0 { -x } else { 0 };
    let y_offset = if y < 0 { -y } else { 0 };
    x += x_offset;
    y += y_offset;

    let size_x = if x + size > root_width {
        root_width - x
    } else {
        size - x_offset
    };
    let size_y = if y + size > root_height {
        root_height - y
    } else {
        size - y_offset
    };

    if size_x == size && size_y == size {
        let mut pixels = Vec::with_capacity((size * size) as usize);
        for row in y..y + size {
            let start = (row * root_width + x) as usize;
            pixels.extend_from_slice(&cache[start..start + size as usize]);
        }
        return (size as u16, pixels);
    }

    // Edge case: pad out-of-bounds pixels with transparent
    let mut pixels = vec![ARGB::TRANSPARENT; (size * size) as usize];
    for cx in 0..size_x {
        for cy in 0..size_y {
            let cache_idx = ((cy + y) * root_width + (cx + x)) as usize;
            let pixels_idx = ((cy + y_offset) * size + (cx + x_offset)) as usize;
            pixels[pixels_idx] = cache[cache_idx];
        }
    }
    (size as u16, pixels)
}

// Draws the magnifier into a pixel buffer using the cached screenshot.
// Pure CPU work — no X11 calls, so no compositor synchronization issues.
fn render_magnifier_from_cache(
    cache: &[ARGB],
    cache_width: u16,
    cache_height: u16,
    point: (i16, i16),
    preview_width: u32,
    scale: u32,
) -> Vec<u32> {
    let (w, screenshot_pixels) =
        get_rect_from_cache(cache, cache_width, cache_height, point, preview_width, scale);
    let screenshot = PixelSquare::new(&screenshot_pixels[..], w.into());

    let mut buffer = vec![0u32; (preview_width * preview_width) as usize];
    {
        let mut pixels = PixelSquare::new(&mut buffer[..], preview_width as usize);

        // pixel_size must be odd and slightly larger than the ratio to avoid out-of-bounds
        // during upscaling in draw_magnifying_glass
        let mut pixel_size = pixels.width() / screenshot.width();
        if pixel_size % 2 == 0 {
            pixel_size += 1;
        } else {
            pixel_size += 2;
        }

        draw_magnifying_glass(&mut pixels, &screenshot, pixel_size);
    }

    buffer
}

// Reinterprets a u32 slice as bytes for put_image. The ARGB u32 layout matches
// X11's 32-bit ZPixmap in native byte order.
fn pixels_as_bytes(pixels: &[u32]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(pixels.as_ptr() as *const u8, pixels.len() * 4) }
}

pub fn wait_for_location(
    conn: &Connection,
    screen: &xproto::Screen,
    preview_width: u32,
    scale: u32,
) -> Result<Option<ARGB>> {
    let root = screen.root();
    let preview_width = preview_width.ensure_odd();
    let escape_keycode = escape_keycode(conn)?;
    let return_keycode = return_keycode(conn)?;

    // Grab with an invisible cursor (hides real cursor, captures events)
    let blank_cursor = create_blank_cursor(conn, screen)?;
    grab_pointer(conn, root, blank_cursor)?;

    // Take a full-screen screenshot while our magnifier window does not exist yet.
    // This cache is used for all magnifier rendering, avoiding the compositor
    // synchronization problem: get_image on root during the event loop would read
    // the compositor's framebuffer which still contains our own magnifier pixels,
    // causing a hall-of-mirrors effect.
    let root_width = screen.width_in_pixels();
    let root_height = screen.height_in_pixels();
    let screenshot_cache = color::window_rect(conn, root, (0, 0, root_width, root_height))?;

    // Find 32-bit TrueColor visual for ARGB window
    let argb_visual =
        find_argb_visual(screen).ok_or_else(|| anyhow!("No 32-bit TrueColor visual found"))?;

    // Create colormap for the ARGB visual
    let colormap = conn.generate_id();
    xproto::create_colormap(
        conn,
        xproto::COLORMAP_ALLOC_NONE as u8,
        colormap,
        root,
        argb_visual,
    );

    // Query initial pointer position
    let pointer = xproto::query_pointer(conn, root).get_reply()?;
    let initial_point = (pointer.root_x(), pointer.root_y());

    // Create override-redirect ARGB window OFF-SCREEN. We map the window immediately
    // and keep it mapped for its entire lifetime — some X servers / compositors discard
    // a window's pixel buffer while it is unmapped, so put_image data would be lost.
    let win = conn.generate_id();
    let win_size = preview_width as u16;
    xproto::create_window(
        conn,
        32,
        win,
        root,
        -10000,
        -10000,
        win_size,
        win_size,
        0,
        xproto::WINDOW_CLASS_INPUT_OUTPUT as u16,
        argb_visual,
        &[
            (xproto::CW_BACK_PIXEL, 0),
            (xproto::CW_BORDER_PIXEL, 0),
            (xproto::CW_OVERRIDE_REDIRECT, 1),
            (xproto::CW_COLORMAP, colormap),
        ],
    );

    // Create GC for the window
    let gc = conn.generate_id();
    xproto::create_gc(conn, gc, win, &[]);

    // Map window off-screen so the server allocates its pixel buffer
    xproto::map_window(conn, win);
    conn.flush();

    // Render initial magnifier from cache and paint into the now-mapped window
    let pixels = render_magnifier_from_cache(
        &screenshot_cache,
        root_width,
        root_height,
        initial_point,
        preview_width,
        scale,
    );
    xproto::put_image(
        conn,
        xproto::IMAGE_FORMAT_Z_PIXMAP as u8,
        win,
        gc,
        win_size,
        win_size,
        0,
        0,
        0,
        32,
        pixels_as_bytes(&pixels),
    );

    // Move window on-screen centered on cursor
    let win_x = initial_point.0 - (win_size as i16) / 2;
    let win_y = initial_point.1 - (win_size as i16) / 2;
    xproto::configure_window(
        conn,
        win,
        &[
            (xproto::CONFIG_WINDOW_X as u16, win_x as u16 as u32),
            (xproto::CONFIG_WINDOW_Y as u16, win_y as u16 as u32),
        ],
    );
    conn.flush();
    grab_keyboard_with_retry(conn, root)?;

    let result = loop {
        let event = conn.wait_for_event();
        if let Some(event) = event {
            match event.response_type() {
                xproto::BUTTON_PRESS => {
                    let event: &xproto::ButtonPressEvent = unsafe { xbase::cast_event(&event) };
                    match event.detail() {
                        SELECTION_BUTTON => {
                            // Read directly from cache — no X11 round-trip needed
                            let x = (event.root_x().max(0) as usize)
                                .min(root_width as usize - 1);
                            let y = (event.root_y().max(0) as usize)
                                .min(root_height as usize - 1);
                            break Some(screenshot_cache[y * root_width as usize + x]);
                        }
                        _ => {}
                    }
                }
                xproto::KEY_PRESS => {
                    let event: &xproto::KeyPressEvent = unsafe { xbase::cast_event(&event) };
                    if event.detail() == escape_keycode {
                        break None;
                    } else if event.detail() == return_keycode {
                        let pointer = xproto::query_pointer(conn, root).get_reply()?;
                        let x = (pointer.root_x().max(0) as usize)
                            .min(root_width as usize - 1);
                        let y = (pointer.root_y().max(0) as usize)
                            .min(root_height as usize - 1);
                        break Some(screenshot_cache[y * root_width as usize + x]);
                    }
                }
                xproto::MOTION_NOTIFY => {
                    let event: &xproto::MotionNotifyEvent =
                        unsafe { xbase::cast_event(&event) };
                    let point = (event.root_x(), event.root_y());

                    // Render from cache — pure CPU, no compositor interaction
                    let pixels = render_magnifier_from_cache(
                        &screenshot_cache,
                        root_width,
                        root_height,
                        point,
                        preview_width,
                        scale,
                    );

                    // Update window content and position
                    let new_x = point.0 - (win_size as i16) / 2;
                    let new_y = point.1 - (win_size as i16) / 2;
                    xproto::put_image(
                        conn,
                        xproto::IMAGE_FORMAT_Z_PIXMAP as u8,
                        win,
                        gc,
                        win_size,
                        win_size,
                        0,
                        0,
                        0,
                        32,
                        pixels_as_bytes(&pixels),
                    );
                    xproto::configure_window(
                        conn,
                        win,
                        &[
                            (xproto::CONFIG_WINDOW_X as u16, new_x as u16 as u32),
                            (xproto::CONFIG_WINDOW_Y as u16, new_y as u16 as u32),
                        ],
                    );
                    conn.flush();
                }
                _ => {}
            }
        } else {
            break None;
        }
    };

    // Cleanup
    xproto::unmap_window(conn, win);
    xproto::destroy_window(conn, win);
    xproto::free_gc(conn, gc);
    xproto::free_colormap(conn, colormap);
    xproto::ungrab_keyboard(conn, xbase::CURRENT_TIME);
    xproto::ungrab_pointer(conn, xbase::CURRENT_TIME);
    xproto::free_cursor(conn, blank_cursor);
    conn.flush();

    Ok(result)
}
