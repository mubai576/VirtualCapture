// WGC resize/lifecycle regression without opening a real capture session.
// The production helpers in sender/wgc.h are exercised directly so the test
// stays deterministic on machines without an interactive desktop or a window
// that can be resized while capturing.
#include <cstdio>
#include <cstring>
#include <string>
#include <chrono>
#include <thread>

#include "wgc.h"
#include "dda.h"
#include "stream.h"

static int g_failures = 0;
#define CHECK(cond) do { \
  if (!(cond)) { std::printf("FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond); ++g_failures; } \
} while (0)

static void test_error_mapping() {
  // The sender treats only E_TIMEOUT as non-fatal (vc_sender.cpp and
  // encoded_sender.h use `!= DDACap::E_TIMEOUT`). Closed/callback errors must
  // therefore be distinct values that fall into the fatal branch.
  CHECK(WGCCap::E_CLOSED != WGCCap::E_TIMEOUT);
  CHECK(WGCCap::E_CALLBACK != WGCCap::E_TIMEOUT);
  CHECK((int)WGCCap::E_CLOSED != (int)DDACap::E_TIMEOUT);
  CHECK((int)WGCCap::E_CALLBACK != (int)DDACap::E_TIMEOUT);
  const auto fatal_for_sender = [](int code) { return code != (int)DDACap::E_TIMEOUT; };
  CHECK(fatal_for_sender(WGCCap::E_CLOSED));
  CHECK(fatal_for_sender(WGCCap::E_CALLBACK));
  CHECK(!fatal_for_sender(WGCCap::E_TIMEOUT));

  // Precedence: a closed item wins over a callback failure, which wins over idle.
  CHECK(WGCCap::no_frame_error(true, false) == WGCCap::E_CLOSED);
  CHECK(WGCCap::no_frame_error(false, true) == WGCCap::E_CALLBACK);
  CHECK(WGCCap::no_frame_error(false, false) == WGCCap::E_TIMEOUT);
  CHECK(WGCCap::no_frame_error(true, true) == WGCCap::E_CLOSED);
}

static void test_content_rect() {
  // Window shrunk_contenter than the frame-pool texture: only ContentSize is defined.
  auto shrunk_content = WGCCap::content_rect(800, 600, 1920, 1080);
  CHECK(shrunk_content.valid());
  CHECK(shrunk_content.w == 800 && shrunk_content.h == 600);

  // Defensive clamp: never address past the texture even if ContentSize is stale.
  auto clamped = WGCCap::content_rect(1920, 1080, 1280, 720);
  CHECK(clamped.w == 1280 && clamped.h == 720);

  // Minimized/empty or degenerate content must not be processed.
  CHECK(!WGCCap::content_rect(0, 600, 1920, 1080).valid());
  CHECK(!WGCCap::content_rect(800, 0, 1920, 1080).valid());
  CHECK(!WGCCap::content_rect(1, 600, 1920, 1080).valid());
  CHECK(!WGCCap::content_rect(800, 600, 0, 1080).valid());
}

static void test_copy_rect_and_bounds() {
  VCStream crop;
  crop.width = 640; crop.height = 640; crop.transform = "crop";
  WGCSourceRect content = WGCCap::content_rect(1920, 1080, 1920, 1080);
  VCTransform t;
  CHECK(t.init(content.w, content.h, crop));
  WGCCopyRect copy = WGCCap::frame_copy_rect(t, crop, content);
  CHECK(copy.valid());
  CHECK(copy.x == 640 && copy.y == 220 && copy.w == 640 && copy.h == 640);
  CHECK(copy.x + copy.w <= content.w && copy.y + copy.h <= content.h);

  // Every sampled output pixel stays inside ContentSize (never the full texture
  // when the window is shrunk_contenter than the pooled frame texture).
  WGCSourceRect shrunk = WGCCap::content_rect(800, 600, 1920, 1080);
  const char* transforms[] = {"stretch", "fit", "fill"};
  for (const char* name : transforms) {
    VCStream s; s.width = 640; s.height = 640; s.transform = name;
    VCTransform tr;
    CHECK(tr.init(shrunk.w, shrunk.h, s));
    int sampled = 0;
    for (uint32_t y = 0; y < 640; y += 37) {
      for (uint32_t x = 0; x < 640; x += 41) {
        uint32_t sx = 0, sy = 0;
        if (tr.sample(x, y, sx, sy)) {
          ++sampled;
          CHECK(sx < (uint32_t)shrunk.w && sy < (uint32_t)shrunk.h);
        }
      }
    }
    CHECK(sampled > 0); // fit letterbox may reject some output pixels, never sample them
  }
  // Crop samples only the centred content window, still inside ContentSize.
  {
    VCStream s; s.width = 640; s.height = 640; s.transform = "crop";
    VCTransform tr;
    CHECK(tr.init(content.w, content.h, s));
    int sampled = 0;
    for (uint32_t y = 0; y < 640; y += 37) {
      for (uint32_t x = 0; x < 640; x += 41) {
        uint32_t sx = 0, sy = 0;
        if (tr.sample(x, y, sx, sy)) {
          ++sampled;
          CHECK(sx >= copy.x && sx < (uint32_t)(copy.x + copy.w));
          CHECK(sy >= copy.y && sy < (uint32_t)(copy.y + copy.h));
        }
      }
    }
    CHECK(sampled > 0);
  }

  // Odd (non-even) content dimensions in stretch mode stay inside the source.
  VCStream odd; odd.width = 640; odd.height = 640; odd.transform = "stretch";
  WGCSourceRect odd_src = WGCCap::content_rect(1001, 701, 1920, 1080);
  VCTransform odd_t;
  CHECK(odd_t.init(odd_src.w, odd_src.h, odd));
  int odd_sampled = 0;
  for (uint32_t y = 0; y < 640; y += 13) {
    for (uint32_t x = 0; x < 640; x += 17) {
      uint32_t sx = 0, sy = 0;
      if (odd_t.sample(x, y, sx, sy)) {
        ++odd_sampled;
        CHECK(sx < 1001 && sy < 701);
      }
    }
  }
  CHECK(odd_sampled > 0);

  // Crop needs enough content; a shrunk window must be rejected rather than
  // silently changing the negotiated output size.
  VCStream s; s.width = 640; s.height = 640; s.transform = "crop";
  VCTransform too_shrunk_content;
  CHECK(!too_shrunk_content.init(600, 600, s));
  CHECK(s.width == 640 && s.height == 640 && s.transform == "crop");
}

static void test_resize_sequence() {
  VCStream stream; stream.width = 640; stream.height = 640; stream.transform = "stretch";

  int pool_w = 1920, pool_h = 1080;
  CHECK(!WGCCap::needs_pool_recreate(1920, 1080, pool_w, pool_h)); // steady state
  CHECK(WGCCap::needs_pool_recreate(2560, 1440, pool_w, pool_h));  // grow
  CHECK(WGCCap::needs_pool_recreate(800, 600, pool_w, pool_h));    // shrink
  CHECK(!WGCCap::needs_pool_recreate(0, 0, pool_w, pool_h));       // minimized: wait
  CHECK(!WGCCap::needs_pool_recreate(1, 1080, pool_w, pool_h));    // transient invalid

  // Model capture()'s ordering with the production helpers. ContentSize is the
  // resize authority (item.Size() may stay at its initial value):
  // 1) any ContentSize change -> recreate the pool to ContentSize, drop frame;
  // 2) content beyond the texture (inconsistent pool) -> reject;
  // 3) empty content -> no drawable pixels;
  // 4) otherwise process only the ContentSize rectangle.
  enum { RECREATE, STALE, EMPTY, PROCESS };
  auto step = [&](int content_w, int content_h, int tex_w, int tex_h,
                  WGCCopyRect& out) -> int {
    if (WGCCap::needs_pool_recreate(content_w, content_h, pool_w, pool_h)) return RECREATE;
    if (content_w > tex_w || content_h > tex_h) return STALE;
    WGCSourceRect src = WGCCap::content_rect(content_w, content_h, tex_w, tex_h);
    if (!src.valid()) return EMPTY;
    VCTransform t;
    if (!t.init(src.w, src.h, stream)) return STALE;
    out = WGCCap::frame_copy_rect(t, stream, src);
    return out.valid() ? PROCESS : STALE;
  };

  WGCCopyRect out{};
  // Grow: ContentSize exceeds the initial pool -> recreate.
  CHECK(step(2560, 1440, 1920, 1080, out) == RECREATE);
  pool_w = 2560; pool_h = 1440; // capture() calls recreate_pool(ContentSize)
  // Shrink: ContentSize is shrunk_contenter than the pool -> still recreate, not process.
  CHECK(step(1600, 900, 2560, 1440, out) == RECREATE);
  pool_w = 1600; pool_h = 900;
  CHECK(step(1600, 900, 1600, 900, out) == PROCESS);
  CHECK(out.x == 0 && out.y == 0 && out.w == 1600 && out.h == 900); // content, not old texture
  CHECK(step(0, 0, 1600, 900, out) == EMPTY);
  CHECK(step(1600, 900, 1200, 800, out) == STALE); // inconsistent texture < ContentSize
  CHECK(step(1600, 900, 1600, 900, out) == PROCESS);
  CHECK(out.x == 0 && out.y == 0 && out.w == 1600 && out.h == 900);

  // Negotiated output is untouched by every resize path above.
  CHECK(stream.width == 640 && stream.height == 640 && stream.transform == "stretch");
}

static void test_lifecycle_teardown() {
  WGCCap cap;

  // CallbackScope is what shutdown() drains before a resize Recreate.
  {
    WGCCap::CallbackScope a(cap.state->inflight);
    WGCCap::CallbackScope b(cap.state->inflight);
    CHECK(cap.state->inflight.load() == 2);
  }
  CHECK(cap.state->inflight.load() == 0);

  // Deterministic teardown ordering: shutdown() revokes the delegates and
  // replaces the shared callback state. A queued delegate keeps the old state
  // alive; the old state (not WGCCap) owns the event/pending frame. The test
  // holds a CallbackScope on the old state to make that lifetime explicit.
  {
    auto old_state = cap.state;
    std::thread in_flight([old_state] {
      WGCCap::CallbackScope scope(old_state->inflight);
      std::this_thread::sleep_for(std::chrono::milliseconds(120));
    });
    std::this_thread::sleep_for(std::chrono::milliseconds(20));
    cap.shutdown(); // replaces cap.state; old_state must stay valid on its own
    CHECK(cap.state != old_state);
    CHECK(old_state->inflight.load() == 1); // the queued callback still owns its state
    in_flight.join();
    CHECK(old_state->inflight.load() == 0);
    CHECK(cap.state->inflight.load() == 0);
  }

  // Shutdown must install a fresh callback state and be idempotent. This runs
  // with no pool/session/item, so it needs no desktop capture.
  cap.state->item_closed.store(true);
  cap.state->callback_failed.store(true);
  cap.state->callback_hr.store(0x12345678);
  cap.state->recreating.store(true);
  cap.shutdown();
  CHECK(!cap.state->item_closed.load());
  CHECK(!cap.state->callback_failed.load());
  CHECK(cap.state->callback_hr.load() == 0);
  CHECK(!cap.state->recreating.load());
  CHECK(cap.state->inflight.load() == 0);
  cap.shutdown();
  CHECK(!cap.state->item_closed.load() && !cap.state->callback_failed.load());

  // Without a frame pool no frame may be reported as processed.
  uint8_t y = 0, uv = 0;
  CHECK(!cap.capture(&y, &uv));
  CHECK(cap.err_code == WGCCap::E_NODDA);
}

int main() {
  test_error_mapping();
  test_content_rect();
  test_copy_rect_and_bounds();
  test_resize_sequence();
  test_lifecycle_teardown();
  if (g_failures == 0) {
    std::printf("WGC regression: PASS (resize/content/error/lifecycle)\n");
    return 0;
  }
  std::printf("WGC regression: FAIL (%d checks)\n", g_failures);
  return 1;
}
