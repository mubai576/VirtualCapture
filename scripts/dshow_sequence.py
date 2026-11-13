"""Order-based DShow output checks shared by the P6 loopback diagnostics.

The DShow pin may repeat the last good frame while the receiver is between
publishes (D12), may start late while the graph initialises, and keeps
repeating the final frame after the sender stops.  A membership-only test
("every DShow hash is somewhere in the decoded log") therefore passes even if
the output is frozen on an old frame while the receiver continues decoding
motion frames.

These checks use only local log order and hashes: no QPC/clock values are
compared, in particular nothing is compared across hosts.  A DShow hash is
matched to a decoded occurrence at or after the previous match; repeating the
current hash holds its occurrence, and changing hash takes the earliest future
occurrence.  That accepts legitimate desktop sequences such as A,B,A even when
the same image reappears.  Repeats are allowed at the startup and trailing
boundaries and for documented short stalls, but the observed output must
advance through the decoded frame sequence and reach the final decoded frame.

Lossy codecs never claim pixel equality with the source here; the association
is only between DShow NV12 sample hashes and receiver decoded NV12 hashes.
"""
from bisect import bisect_left
from collections.abc import Mapping


class SequenceError(ValueError):
    """DShow samples do not form a valid progression of decoded frames."""


def _hash(row, key, side):
    if not isinstance(row, Mapping) or key not in row:
        raise SequenceError("{} row is missing {!r}".format(side, key))
    value = row[key]
    if not isinstance(value, str) or not value:
        raise SequenceError("{} row has an invalid {!r}: {!r}".format(side, key, value))
    return value


def _decoded_occurrences(decoded, key):
    """Map each decoded hash to every occurrence index, in decode order."""
    occurrences = {}
    for index, row in enumerate(decoded):
        occurrences.setdefault(_hash(row, key, "decoded"), []).append(index)
    return occurrences


def _runs(values):
    """(value, length) for each maximal equal run, in order."""
    runs = []
    for value in values:
        if runs and runs[-1][0] == value:
            runs[-1][1] += 1
        else:
            runs.append([value, 1])
    return [(value, length) for value, length in runs]


def verify_dshow_progression(decoded, video, *, decoded_hash_key, require_motion,
                             fps=None, max_stall_samples=None, max_terminal_lag=0):
    """Validate that DShow samples advance through the decoded frame order.

    ``decoded`` is the receiver hashlog (in decode order); ``video`` is the
    DShow hashlog.  ``decoded_hash_key`` selects ``decoded_hash`` for encoded
    streams or ``hash`` for Raw.  Rows may carry timestamps, but they are not
    used.

    ``require_motion`` must be True only when the source is known to change
    every frame (the ``motion`` pattern).  It then also requires the decoded
    rows to be pairwise distinct, so a decoder that reuses frames (or a hash
    collision) cannot be mistaken for progress.  DDA/WGC output may
    legitimately repeat frames or stay static (including A,B,A returns), so its
    motion gate is skipped; association, ordering and terminal reach are still
    enforced.

    ``max_terminal_lag`` permits the DShow verifier to stop a bounded number of
    decoded frames before the receiver (product tests stop the graph before the
    capture side); the default requires the final decoded frame.

    Raises :class:`SequenceError` on a frozen/incomplete/out-of-order output.
    Returns metrics suitable for the diagnostic result JSON.
    """
    decoded = list(decoded)
    video = list(video)
    if not decoded:
        raise SequenceError("receiver decoded no frames")
    if not video:
        raise SequenceError("DShow produced no samples")

    occurrences = _decoded_occurrences(decoded, decoded_hash_key)
    distinct = len(occurrences)
    if require_motion:
        if distinct < 2:
            raise SequenceError("decoded motion frames do not progress: "
                                "{} rows contain only {} distinct hash(es)".format(len(decoded), distinct))
        if distinct != len(decoded):
            raise SequenceError("decoded motion frames are not distinct: {} rows contain {} hashes "
                                "(hash collision or a frame that never changed)".format(
                                    len(decoded), distinct))

    # DShow sample numbers are written in Receive order; a gap or regression
    # means the hashlog itself is not a usable sequence.
    samples = []
    for index, row in enumerate(video):
        sample = row.get("sample") if isinstance(row, Mapping) else None
        if type(sample) is not int:
            raise SequenceError("DShow row {} has no integer sample index".format(index))
        if samples and sample != samples[-1] + 1:
            raise SequenceError("DShow sample sequence is not contiguous: {} then {}".format(
                samples[-1], sample))
        samples.append(sample)

    # Non-decreasing decoded occurrence indices.  A repeated hash holds the
    # current occurrence (D12 repeats the last good frame); a changed hash must
    # match its earliest occurrence at or after the previous one.
    indices = []
    previous_value = previous_index = None
    for index, row in enumerate(video):
        value = _hash(row, "hash", "DShow")
        positions = occurrences.get(value)
        if positions is None:
            raise SequenceError("DShow sample {} hash {} is absent from the decoded hashlog".format(
                samples[index], value))
        if value == previous_value:
            position = previous_index
        else:
            start = 0 if previous_index is None else previous_index
            slot = bisect_left(positions, start)
            if slot == len(positions):
                raise SequenceError("DShow hash order regressed: hash {} has no occurrence at or "
                                    "after decoded index {}".format(value, start))
            position = positions[slot]
        indices.append(position)
        previous_value, previous_index = value, position

    # Terminal reach uses the last occurrence of the final DShow hash, so a
    # repeated final image is not mistaken for falling short of the tail.
    terminal_index = len(decoded) - 1
    terminal_lag = terminal_index - occurrences[previous_value][-1]
    if terminal_lag > max_terminal_lag:
        raise SequenceError("DShow output stopped at decoded index {} but the receiver decoded "
                            "through index {} (lag {}, allowed {}; frozen or truncated output)".format(
                                indices[-1], terminal_index, terminal_lag, max_terminal_lag))

    runs = _runs(indices)
    leading = runs[0][1] - 1 if len(runs) > 1 else len(indices) - 1
    trailing = runs[-1][1] - 1 if len(runs) > 1 else 0
    interior = runs[1:-1]
    max_stall = max((length for _, length in interior), default=0)
    if max_stall_samples is None:
        max_stall_samples = max(2, round(fps)) if fps else 2
    # A long repeat is only evidence of a freeze when the source is known to
    # change every frame; a static DDA/WGC desktop is allowed to stall.
    if require_motion and max_stall > max_stall_samples:
        raise SequenceError("DShow output stalled for {} samples (limit {}); repeats are allowed "
                            "but not a sustained freeze".format(max_stall, max_stall_samples))

    # Active samples exclude the constant startup and trailing runs.  Motion
    # needs a real index advance; skipping frames is a throughput observation,
    # not a freeze, so no coverage ratio is required.
    start = leading
    end = len(indices) - trailing
    active = indices[start:end]
    active_distinct = len(set(active))
    span = active[-1] - active[0] + 1 if active else 0
    if require_motion and (active_distinct < 2 or span < 2):
        raise SequenceError("DShow did not show motion progress: {} distinct decoded index(es) "
                            "over span {} (frozen output)".format(active_distinct, span))

    return {
        "samples": len(indices),
        "decoded_frames": len(decoded),
        "decoded_distinct_hashes": distinct,
        "ordered": True,
        "terminal_index": terminal_index,
        "terminal_lag": terminal_lag,
        "terminal_hash_reached": terminal_lag == 0,
        "leading_repeats": leading,
        "trailing_repeats": trailing,
        "active_samples": len(active),
        "active_distinct_indexes": active_distinct,
        "active_progress_frames": span,
        "active_coverage": active_distinct / span if span else 0.0,
        "max_stall_repeats": max_stall,
        "stall_limit": max_stall_samples,
        "association": "dshow_hash_in_decoded_hashlog",
        "ordering": "non_decreasing_decoded_occurrence_index",
        "pixel_equality_gate": False,
    }
