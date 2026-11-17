// C ABI exported by shim.m. Mirrored in src/ffi.rs.
#pragma once
#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>

typedef struct {
    uint8_t *data;   // RGBA8, non-premultiplied, row-major, malloc'd
    int32_t width;
    int32_t height;
} bugle_image_t;

typedef struct {
    uint32_t id;
    double x, y, w, h;   // points, global top-left origin
    double scale;
    int32_t is_main;
} bugle_display_t;

typedef struct {
    uint32_t id;
    int32_t pid;
    double x, y, w, h;
    int32_t layer;
    int32_t on_screen;
    char owner[256];
    char title[512];
    double alpha;        // kCGWindowAlpha, 1.0 when absent
} bugle_window_t;

typedef struct {
    char *text;
    double x, y, w, h;   // pixels, top-left origin
    double confidence;
} bugle_ocr_line_t;

void bugle_free(void *p);
void bugle_image_free(bugle_image_t *img);
// The calling thread's last error, handed over (free with bugle_free) and
// cleared; NULL when none. Errors are per thread: read it on the thread that
// made the failing call, before calling into the shim again.
char *bugle_last_error(void);

int32_t bugle_screen_access_preflight(void);
int32_t bugle_screen_access_request(void);
int32_t bugle_accessibility_trusted(int32_t prompt);

int32_t bugle_list_displays(bugle_display_t *out, int32_t max);
// Writes up to `max` windows and returns the total count found (may exceed
// `max`), or -1 on failure. `out` may be NULL when `max` is 0.
int32_t bugle_list_windows(bugle_window_t *out, int32_t max, int32_t on_screen_only);

bugle_image_t bugle_capture_display(uint32_t display_id, int32_t show_cursor);
// Rejects non-finite rects and sides outside (0, 16384] points.
bugle_image_t bugle_capture_area(double x, double y, double w, double h, int32_t show_cursor);
bugle_image_t bugle_capture_window(uint32_t window_id, int32_t shadow, int32_t show_cursor);

// `len` is the byte length of `rgba`; it must be at least w * h * 4.
int32_t bugle_ocr(const uint8_t *rgba, size_t len, int32_t w, int32_t h, const char *langs_csv,
                  int32_t accurate, int32_t lang_correction,
                  bugle_ocr_line_t **out_lines, int32_t *out_count);
void bugle_ocr_free(bugle_ocr_line_t *lines, int32_t count);

int32_t bugle_register_translation(const uint8_t *ref_rgba, size_t ref_len, int32_t rw, int32_t rh,
                                   const uint8_t *flt_rgba, size_t flt_len, int32_t fw, int32_t fh,
                                   double *dx, double *dy);

int32_t bugle_post_scroll(double x, double y, int32_t dx_lines, int32_t dy_lines);
void bugle_cursor_position(double *x, double *y);
// Pressed mouse buttons: bit 0 left, 1 right, 2 middle.
int32_t bugle_mouse_buttons(void);
// Seconds on the host clock (CMClockGetHostTimeClock), which stamps
// ScreenCaptureKit samples.
double bugle_host_time_now(void);
// Current frame of one window (global points); 0 ok, -1 when it is gone.
int32_t bugle_window_frame(uint32_t window_id, double *x, double *y, double *w, double *h);
// 1 when child `pid` has exited (not reaped, so its owner can still wait), 0 if not, -1 on error.
int32_t bugle_child_exited(int32_t pid);

int32_t bugle_clipboard_write_image(const uint8_t *png, size_t len);
int32_t bugle_clipboard_write_text(const char *text);
int32_t bugle_clipboard_write_file(const char *path);

int32_t bugle_select_rect_blocking(double *x, double *y, double *w, double *h);
// Non-blocking variant for hosts that already run the AppKit loop: returns 1
// and later calls `cb` on the main thread (ok=0 when cancelled), or 0 when a
// selection or window picker is already up (cb is then never called).
typedef void (*bugle_select_callback_t)(void *ctx, int32_t ok, double x, double y, double w, double h);
int32_t bugle_select_rect_begin(bugle_select_callback_t cb, void *ctx);
// Like bugle_select_rect_begin, plus: with `allow_switch`, Space (while not
// dragging) ends the selection with ok=2 and leaves Bugle active, so the host
// can open the window picker; `timeout_seconds` > 0 cancels (ok=0) after that
// long.
int32_t bugle_select_rect_begin_ex(bugle_select_callback_t cb, void *ctx, int32_t allow_switch, double timeout_seconds);
// What Space does (ok=2), when allowed and not dragging; no-op otherwise.
void bugle_select_rect_switch(void);
// [NSApp deactivate]: hand focus back after a switch that led nowhere.
void bugle_app_deactivate(void);
// A loop of the caller's own (the CLI): bootstrap NSApplication once, then
// pump events for up to `seconds` per call. Main thread only.
void bugle_app_bootstrap(void);
void bugle_run_loop_once(double seconds);
int32_t bugle_is_main_thread(void);
// Draws a one-line hint centred near the bottom of an overlay view (shared by
// the selection overlay and the window picker).
#ifdef __OBJC__
#import <AppKit/AppKit.h>
void bugle_draw_overlay_hint(NSRect bounds, NSString *text);
// The newest open markup editor's window, and the video editor's, or nil.
NSWindow *bugle_markup_editor_front_window(void);
NSWindow *bugle_video_editor_window(void);
#endif
// Called by an editor window (markup or video) once it is off screen: puts
// the keyboard focus on another open Bugle editor, or, when none is left,
// hands it back to the app the user came from. Main thread only.
void bugle_editor_window_closed(void);
// The title of Bugle's key window (UTF-8, truncated to `cap - 1` bytes and
// NUL-terminated); 0 when Bugle is not the active app or has no key window. Any thread: hops to the
// main thread, so never call it from the main thread's blocked waiters.
int32_t bugle_key_window_title(char *buf, size_t cap);
#ifdef __OBJC__
#endif
int32_t bugle_select_rect_active(void);
void bugle_select_rect_cancel(void);
void bugle_pin_image_blocking(const uint8_t *png, size_t len, double x, double y, double w, double h, double opacity);
int32_t bugle_pin_image_show(const uint8_t *png, size_t len, double x, double y, double w, double h, double opacity);
void bugle_toast_show(const uint8_t *png, size_t len, const char *title, const char *subtitle, const char *open_path,
                      double seconds);

int32_t bugle_export_audio_m4a(const char *in_path, const char *out_path);
int32_t bugle_export_mp4(const char *in_path, const char *out_path);
int32_t bugle_export_mov(const char *in_path, const char *out_path);
int32_t bugle_audio_track_count(const char *path);
// Peak level of all audio tracks mixed, in `n` equal buckets of `duration`
// seconds. 1 filled, 0 no audio, 2 stopped because `*cancel` (may be NULL)
// became true, -1 error.
int32_t bugle_audio_peaks(const char *path, double duration, float *out, int32_t n, const volatile bool *cancel);
// Processes one block of aligned audio at `sample_rate`: `tracks[i]` holds
// `frames` interleaved frames of `channels` floats for track i; writes
// `n_out` runs of as many frames into `out`, one after the other (output
// track k at `out + k * frames * channels`). Non-zero aborts.
typedef int32_t (*bugle_mix_fn)(void *ctx, const float *const *tracks, int32_t n_tracks, int32_t channels,
                                int32_t sample_rate, float *out, int32_t n_out, int32_t frames);
// Re-encode all audio tracks through `process` (video copied as is): into
// one AAC track, or with `separate` into one AAC track per source track.
// 0 on success, 1 (nothing written) with fewer than two audio tracks.
int32_t bugle_mix_audio_tracks(const char *in_path, const char *out_path, int32_t separate, bugle_mix_fn process,
                               void *ctx);
int32_t bugle_video_track_count(const char *path);

// Test fixtures (shim/fixtures.m, its own object file: nothing in the app
// references it, so the linker leaves it out of shipped binaries). Writes an
// .mp4 of `tracks` AAC tracks lasting `seconds`; tracks after the first start
// `delay` seconds in. Each track plays a sine of `amplitude`: with `overlap`
// all tracks play the same 440 Hz tone throughout, otherwise track i plays
// 440*(i+1) Hz during the i-th equal slice only. `video` adds a 64x64 H.264
// track at 10 fps. Returns a malloc'd error (free with bugle_free), NULL on
// success.
char *bugle_fixture_write_tones(const char *out_path, int32_t tracks, double seconds, double amplitude, int32_t overlap,
                                double delay, int32_t video);
double bugle_media_duration(const char *path);
bugle_image_t bugle_video_frame(const char *path, double seconds);

// Editing. `bugle_export_video` exports [start, end) seconds of a movie with an
// AVAssetExportSession preset (0 passthrough, 1 highest quality H.264, 2 HEVC,
// 3 1920x1080, 4 1280x720, 5 960x540) to .mp4 (`mp4` != 0) or .mov; `fps` > 0
// resamples through a video composition (refused with passthrough). Waits at
// most `timeout` seconds (<= 0: no limit), then cancels the export. The frame
// reader decodes frames at arbitrary times (`tolerance` seconds either side);
// the frame writer encodes RGBA frames (even sizes) to H.264 or HEVC at a
// fixed rate.
typedef struct {
    int32_t width, height;      // display size (pixels, after rotation)
    double nominal_fps;
    double start, duration;     // video track's time range, seconds
    int64_t stored_samples;     // samples in the media (incl. edit-list-hidden ones); -1 unknown
} bugle_video_info_t;
int32_t bugle_video_info(const char *path, bugle_video_info_t *out);
int32_t bugle_video_sync_at(const char *path, double seconds);   // 1 when a cut there needs no re-encode
int32_t bugle_export_video(const char *in_path, const char *out_path, double start, double end, int32_t preset,
                           int32_t mp4, int32_t fps, double timeout);
void *bugle_frames_open(const char *path, double tolerance);
bugle_image_t bugle_frames_at(void *handle, double seconds);
void bugle_frames_set_max_size(void *handle, uint32_t max_w, uint32_t max_h);
void bugle_frames_close(void *handle);
void *bugle_writer_open(const char *path, int32_t w, int32_t h, int32_t fps, int32_t mp4, int32_t hevc);
int32_t bugle_writer_append(void *handle, const uint8_t *rgba, size_t len, int32_t w, int32_t h);
int32_t bugle_writer_finish(void *handle, double timeout);   // finalises and releases the handle
void bugle_writer_cancel(void *handle);      // discards the file and releases the handle
// Sequential decoder over [start, end) (end <= start: the whole track).
// `next` returns frames in presentation order with their time in `pts`; a
// NULL image with no error pending is the end of the range.
void *bugle_seq_open(const char *path, double start, double end);
bugle_image_t bugle_seq_next(void *handle, double *pts);
void bugle_seq_close(void *handle);
// Video track of `video_path` + audio of `audio_path` over [start, end),
// copied into one movie. 0 ok, 1 nothing written (no audio there), -1 error.
int32_t bugle_mux_audio(const char *video_path, const char *audio_path, double start, double end, const char *out_path,
                        int32_t mp4, double timeout);
void bugle_play_sound(const char *name);
void bugle_open_path(const char *path);

// ScreenCaptureKit recorder: H.264 video + AAC system audio (+ AAC microphone
// as a second audio track) written straight to an .mp4. `window_id` 0 records
// the rect; `show_clicks` draws mouse clicks into the frames (macOS 15+, a
// warning otherwise); `max_seconds` <= 0 disables the auto-stop. Stop
// finalises the file and releases the handle.
int32_t bugle_sck_record_available(void);
void *bugle_sck_record_start(double x, double y, double w, double h, uint32_t window_id, int32_t fps, int32_t retina,
                             int32_t system_audio, int32_t microphone, int32_t show_cursor, int32_t show_clicks,
                             double max_seconds, const char *out_path);
int64_t bugle_sck_record_frames(void *handle);
// Host-clock time (seconds) of the movie's time 0; NaN before it starts.
double bugle_sck_record_session_start(void *handle);
char *bugle_sck_record_warnings(void *handle);   // malloc'd, NULL when none
int32_t bugle_sck_record_stop(void *handle);

// Window picker overlay (picker.m). `cb` gets kind 0 for every pointer move
// (global top-left points), then exactly one final call: 1 = clicked at
// (x, y), 2 = cancelled (Esc, right click, timeout, Bugle losing active
// status, bugle_pick_cancel), 3 = Space pressed with `allow_switch` set (the
// host opens the area selection instead; Bugle stays active). The
// final call arrives on a later main-queue turn, after the overlay is gone.
// Returns 0 (cb never called) when a picker or selection is already active.
// Main thread only, needs a running AppKit loop.
typedef void (*bugle_pick_callback_t)(void *ctx, int32_t kind, double x, double y);
int32_t bugle_pick_begin(bugle_pick_callback_t cb, void *ctx, double timeout_seconds, int32_t allow_switch);
// Highlight a global top-left rect with a centred label; w or h <= 0 clears.
void bugle_pick_highlight(double x, double y, double w, double h, const char *label);
int32_t bugle_pick_active(void);
void bugle_pick_cancel(void);
// What Space does (kind 3), when the picker was opened with allow_switch.
void bugle_pick_switch(void);
