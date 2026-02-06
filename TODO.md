# TODO

## Cursor Flicker on Startup

**Issue:** The magnifier cursor briefly appears with its top-left corner at the mouse position (hotspot 0,0) before quickly correcting to center at mouse position (hotspot 127,127). This causes a visible flicker/jump on startup.

**Characteristics:**
- Intermittent (not every run)
- Happens with and without compositor (picom)
- Not affected by timing delays
- Cursor ID reuse is not the cause
- Struct layout is correct, hotspot values are correct (127,127)
- Appears to be fundamental X server behavior during pointer grab

**Attempted fixes that did NOT work:**
- Adding delays (50ms, 200ms) before grab
- XSync after cursor creation
- Setting cursor on root window before grab
- Creating dummy cursor first to get fresh ID
- Removing compositor

**Potential solutions to try:**

1. **Pure XCB Render Extension** - Rewrite cursor creation using XCB's render extension directly (`xcb_render_create_cursor`), bypassing Xcursor/Xlib entirely. This uses a different code path and might avoid the issue.

2. **Window-based approach** - Replace the cursor with an actual override-redirect window that follows the mouse. This completely bypasses the X11 cursor system but requires significant rewrite.

3. **Report as Xorg bug** - This might be a known issue in the X server's handling of cursor hotspots during pointer grab.
