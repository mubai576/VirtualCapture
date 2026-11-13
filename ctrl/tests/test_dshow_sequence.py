"""DShow progression checks: frozen output must fail, documented repeats must pass."""
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from dshow_sequence import SequenceError, verify_dshow_progression  # noqa: E402


def decoded_rows(count, key="decoded_hash"):
    # Deliberately no qpc/timestamps: the check is order-based, not clock-based.
    return [{"frame_id": frame, key: "h%02d" % frame} for frame in range(count)]


def video_rows(ranks):
    return [{"sample": index, "hash": "h%02d" % rank} for index, rank in enumerate(ranks)]


def check(decoded, video, *, key="decoded_hash", require_motion=True, fps=30, **kwargs):
    return verify_dshow_progression(decoded, video, decoded_hash_key=key,
                                    require_motion=require_motion, fps=fps, **kwargs)


def test_frozen_dshow_output_is_rejected():
    # Receiver keeps decoding 10 motion frames while DShow repeats frame 0.
    with pytest.raises(SequenceError, match="stopped at decoded index|frozen"):
        check(decoded_rows(10), video_rows([0] * 30))


def test_frozen_output_on_the_final_hash_is_still_rejected():
    # Terminal reach must not substitute for progress: a constant stream of the
    # last decoded hash passes association and terminal reach but never moves.
    with pytest.raises(SequenceError, match="did not show motion progress"):
        check(decoded_rows(10), video_rows([9] * 30))


def test_progressing_frames_with_stall_repeats_are_accepted():
    ranks = [0, 1, 2, 3, 3, 4, 5, 6, 7, 8, 9] + [9] * 50
    metrics = check(decoded_rows(10), video_rows(ranks))
    assert metrics["terminal_hash_reached"] is True
    assert metrics["active_progress_frames"] == 10
    assert metrics["active_distinct_indexes"] == 10
    assert metrics["trailing_repeats"] == 50
    assert metrics["pixel_equality_gate"] is False


def test_dshow_order_regression_is_rejected():
    ranks = [0, 1, 2, 3, 5, 4, 6, 7, 8, 9]
    with pytest.raises(SequenceError, match="order regressed"):
        check(decoded_rows(10), video_rows(ranks))


def test_non_contiguous_sample_numbers_are_rejected():
    video = video_rows(list(range(10)))
    video[5]["sample"] = 7
    with pytest.raises(SequenceError, match="not contiguous"):
        check(decoded_rows(10), video)


def test_valid_hash_association_is_accepted_and_unknown_hash_rejected():
    metrics = check(decoded_rows(10), video_rows(list(range(10))))
    assert metrics["association"] == "dshow_hash_in_decoded_hashlog"
    video = video_rows(list(range(10)))
    video[5]["hash"] = "deadbeefdeadbeef"
    with pytest.raises(SequenceError, match="absent from the decoded hashlog"):
        check(decoded_rows(10), video)


def test_late_startup_boundary_is_accepted():
    # DShow graph initialisation can start after the receiver already decoded
    # frames; leading repeats of the first observed frame are not motion.
    ranks = [7] * 4 + [8, 9] + [9] * 20
    metrics = check(decoded_rows(10), video_rows(ranks))
    assert metrics["leading_repeats"] == 3
    assert metrics["active_progress_frames"] == 3


def test_output_that_stops_before_receiver_finishes_is_rejected():
    # Progress then freeze at rank 5 while the receiver decoded through rank 9.
    with pytest.raises(SequenceError, match="stopped at decoded index 5"):
        check(decoded_rows(10), video_rows(list(range(6)) + [5] * 30))


def test_small_terminal_lag_is_boundary_configurable():
    ranks = list(range(9)) + [8] * 10
    with pytest.raises(SequenceError, match="stopped at decoded index 8"):
        check(decoded_rows(10), video_rows(ranks))
    metrics = check(decoded_rows(10), video_rows(ranks), max_terminal_lag=1)
    assert metrics["terminal_lag"] == 1
    assert metrics["terminal_hash_reached"] is False


def test_sustained_middle_stall_is_rejected_but_short_ones_pass():
    short = [0, 1, 1, 1, 2, 3, 4, 5, 6, 7, 8, 9]
    assert check(decoded_rows(10), video_rows(short), fps=30)["max_stall_repeats"] == 3
    long = [0, 1] + [1] * 40 + list(range(2, 10))
    with pytest.raises(SequenceError, match="stalled for 41 samples"):
        check(decoded_rows(10), video_rows(long), fps=30)


def test_loss_gap_and_raw_hash_key_use_decoded_order():
    # Loss removes frame 20; DShow follows the decoded sequence, not frame ids.
    frames = list(range(20)) + list(range(21, 31))
    decoded = [{"frame_id": frame, "hash": "h%02d" % rank} for rank, frame in enumerate(frames)]
    metrics = check(decoded, video_rows(list(range(30)) + [29] * 5), key="hash")
    assert metrics["decoded_frames"] == 30
    assert metrics["terminal_index"] == 29


def test_decoder_that_never_progresses_is_rejected():
    decoded = [{"frame_id": frame, "decoded_hash": "same"} for frame in range(10)]
    with pytest.raises(SequenceError, match="do not progress"):
        check(decoded, video_rows([0] * 10))


def test_nonunique_decoded_frames_are_rejected_for_motion():
    # Two frame ids sharing one hash: duplicate pixels or a hash collision.
    # Membership alone would still pass, so the motion gate must reject it.
    decoded = decoded_rows(10)
    decoded[5]["decoded_hash"] = decoded[4]["decoded_hash"]
    with pytest.raises(SequenceError, match="not distinct.*collision"):
        check(decoded, video_rows(list(range(10))))


def test_static_dda_output_is_not_treated_as_motion_freeze():
    # DDA/WGC may legitimately deliver the same desktop image; require_motion=False
    # keeps association/ordering/terminal checks but drops the progress gate.
    decoded = [{"frame_id": frame, "hash": "same"} for frame in range(30)]
    video = [{"sample": index, "hash": "same"} for index in range(60)]
    metrics = check(decoded, video, key="hash", require_motion=False, fps=10)
    assert metrics["active_progress_frames"] == 1
    with pytest.raises(SequenceError, match="do not progress"):
        check(decoded, video, key="hash", require_motion=True, fps=10)


def test_dda_interior_static_stretch_is_allowed():
    # A desktop that changes, then stays static for several seconds, then changes
    # again is DDA-like; only a known motion source treats that as a freeze.
    decoded = [{"frame_id": frame, "hash": "h%02d" % (frame // 5)} for frame in range(30)]
    ranks = [rank for rank in range(6) for _ in range(40)]
    video = [{"sample": index, "hash": "h%02d" % rank} for index, rank in enumerate(ranks)]
    metrics = check(decoded, video, key="hash", require_motion=False, fps=10)
    assert metrics["max_stall_repeats"] == 40
    with pytest.raises(SequenceError, match="stalled for 40 samples"):
        check(decoded_rows(6), video, fps=10)


def test_dda_return_to_earlier_image_is_accepted():
    # Desktop sequence A,B,A,B,A legitimately revisits images.  First-appearance
    # ranks would call the return to A a regression; occurrence indices must not.
    hashes = ["A", "B", "A", "B", "A"]
    decoded = [{"frame_id": index, "hash": value} for index, value in enumerate(hashes)]
    video = [{"sample": index, "hash": value} for index, value in enumerate(hashes)]
    metrics = check(decoded, video, key="hash", require_motion=False, fps=10)
    assert metrics["active_progress_frames"] == 5
    assert metrics["ordering"] == "non_decreasing_decoded_occurrence_index"
    # The same sequence with stall repeats of the current image is still valid.
    repeated = [{"sample": index, "hash": value}
                for index, value in enumerate(["A", "B", "B", "A", "A", "B", "A"])]
    assert check(decoded, repeated, key="hash", require_motion=False, fps=10)["terminal_lag"] == 0
    abab = ["A", "B", "A", "B"]
    decoded_abab = [{"frame_id": index, "hash": value} for index, value in enumerate(abab)]
    video_abab = [{"sample": index, "hash": value} for index, value in enumerate(abab + ["B", "B"])]
    assert check(decoded_abab, video_abab, key="hash", require_motion=False, fps=10)[
        "active_progress_frames"] == 4


def test_dda_wrong_order_is_rejected():
    # Decoded A,B,A; returning to B after the final A has no future occurrence.
    decoded = [{"frame_id": index, "hash": value} for index, value in enumerate(["A", "B", "A"])]
    video = [{"sample": index, "hash": value} for index, value in enumerate(["B", "A", "B"])]
    with pytest.raises(SequenceError, match="order regressed"):
        check(decoded, video, key="hash", require_motion=False, fps=10)


def test_bitrate_and_max_bitrate_are_independent_bounds():
    import p6_codec_verify
    # Legacy target may exceed max: current backends ignore the target.
    p6_codec_verify.validate_case(416, 416, 30, 60, 8, False, bitrate=80, max_bitrate=20)
    for kwargs in ({"bitrate": 0}, {"bitrate": 2001}, {"max_bitrate": 0}, {"max_bitrate": 2001}):
        with pytest.raises(ValueError):
            p6_codec_verify.validate_case(416, 416, 30, 60, 8, False, **kwargs)
