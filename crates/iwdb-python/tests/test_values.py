"""Python values to database values and back, exactly
(documentation/python-api.md, "Values")."""

import datetime
import math
import struct

import pytest
from hypothesis import HealthCheck, given, settings
from hypothesis import strategies as st

import iwdb


def round_trip(store, value):
    with store.transaction() as tx:
        tx.upsert_node("v", attr={"v": value})
    return store.node("v")["attr"]["v"]


def same(a, b):
    """Equal, with the same types throughout: floats bit for bit (so NaN
    equals NaN and -0.0 differs from 0.0), datetimes with the same UTC
    offset."""
    if type(a) is not type(b):
        return False
    if isinstance(a, float):
        return struct.pack("<d", a) == struct.pack("<d", b)
    if isinstance(a, list):
        return len(a) == len(b) and all(same(x, y) for x, y in zip(a, b))
    if isinstance(a, dict):
        return a.keys() == b.keys() and all(same(a[k], b[k]) for k in a)
    if isinstance(a, datetime.datetime):
        return a == b and a.utcoffset() == b.utcoffset()
    return a == b


@pytest.mark.parametrize(
    "value",
    [
        None,
        True,
        False,
        0,
        1,
        -(2**63),
        2**63 - 1,
        0.0,
        -0.0,
        1.5,
        float("inf"),
        float("-inf"),
        5e-324,
        "",
        "héllo wörld ✓ \U0001f600",
        b"",
        b"\x00\xff\x7f",
        [],
        [1, "a", None, [True, [2.5]]],
        {},
        {"a": {"b": {"c": [1, {}]}}},
        datetime.date(2026, 10, 1),
        datetime.date(1, 1, 1),
        datetime.date(9999, 12, 31),
        datetime.datetime(2026, 10, 1, 12, 30, 15, 250),
        datetime.datetime(2026, 10, 1, 12, 30, tzinfo=datetime.timezone.utc),
        datetime.datetime(2026, 10, 1, 12, 30, tzinfo=datetime.timezone(datetime.timedelta(hours=-5, minutes=-30))),
        datetime.datetime(2026, 10, 1, tzinfo=datetime.timezone(datetime.timedelta(seconds=45))),
    ],
)
def test_values_round_trip_exactly(store, value):
    assert same(round_trip(store, value), value)


def test_nan_and_negative_zero(store):
    assert math.isnan(round_trip(store, float("nan")))
    assert math.copysign(1.0, round_trip(store, -0.0)) == -1.0


def test_bool_stays_bool_and_int_stays_int(store):
    assert round_trip(store, True) is True
    assert type(round_trip(store, 1)) is int
    assert type(round_trip(store, 1.0)) is float


def test_bytearray_reads_back_as_bytes(store):
    assert round_trip(store, bytearray(b"ab")) == b"ab"
    assert type(round_trip(store, bytearray(b"ab"))) is bytes


def test_dicts_read_back_with_keys_sorted(store):
    assert list(round_trip(store, {"b": 1, "a": 2, "c": 3})) == ["a", "b", "c"]


def test_an_aware_datetime_keeps_its_offset_as_a_fixed_timezone(store):
    value = datetime.datetime(2026, 3, 29, 3, 0, tzinfo=datetime.timezone(datetime.timedelta(hours=2), "CEST"))
    back = round_trip(store, value)
    assert back == value and back.utcoffset() == datetime.timedelta(hours=2)


@pytest.mark.parametrize(
    "value, error",
    [
        (2**63, OverflowError),
        (-(2**63) - 1, OverflowError),
        ((1, 2), TypeError),
        ({1, 2}, TypeError),
        ({1: "a"}, TypeError),
        (object(), TypeError),
        (datetime.timedelta(1), TypeError),
        (
            datetime.datetime(2026, 1, 1, tzinfo=datetime.timezone(datetime.timedelta(microseconds=1))),
            ValueError,
        ),
    ],
)
def test_values_that_cant_be_stored(store, value, error):
    with pytest.raises(error):
        with store.transaction() as tx:
            tx.upsert_node("v", attr={"v": value})
    assert store.seq() == 0


def nested(depth, inner):
    """`inner` inside `depth` lists."""
    value = inner
    for _ in range(depth):
        value = [value]
    return value


def test_the_depth_limit(store):
    # A scalar is depth 1: 99 lists around a scalar is depth 100
    assert same(round_trip(store, nested(99, 1)), nested(99, 1))
    with pytest.raises(ValueError, match="100 levels"):
        round_trip(store, nested(100, 1))
    # An empty list is a value like a scalar
    assert same(round_trip(store, nested(99, [])), nested(99, []))
    assert same(round_trip(store, nested(99, {})), nested(99, {}))
    with pytest.raises(ValueError):
        round_trip(store, nested(100, []))
    # A list that contains itself
    loop = []
    loop.append(loop)
    with pytest.raises(ValueError):
        round_trip(store, loop)
    # An appended value sits one level down
    with pytest.raises(ValueError):
        with store.transaction() as tx:
            tx.append_attr("v", "list", nested(99, 1))


def test_reserved_keys_are_refused(store):
    with pytest.raises(iwdb.InvalidError, match="reserved"):
        with store.transaction() as tx:
            tx.upsert_node("v", attr={"iwdb.x": 1})
    with pytest.raises(iwdb.InvalidError, match="reserved"):
        with store.transaction() as tx:
            tx.upsert_node("v", meta={"iwdb.version": 1})


scalars = (
    st.none()
    | st.booleans()
    | st.integers(min_value=-(2**63), max_value=2**63 - 1)
    | st.floats(allow_nan=True, allow_infinity=True)
    | st.text()
    | st.binary()
    | st.dates()
    | st.datetimes()
    | st.datetimes(
        timezones=st.integers(min_value=-86399, max_value=86399).map(
            lambda s: datetime.timezone(datetime.timedelta(seconds=s))
        )
    )
)
values = st.recursive(
    scalars, lambda inner: st.lists(inner, max_size=4) | st.dictionaries(st.text(max_size=5), inner, max_size=4), max_leaves=20
)


@settings(max_examples=300, deadline=None, suppress_health_check=[HealthCheck.function_scoped_fixture])
@given(attr=st.dictionaries(st.text(min_size=1).filter(lambda k: not k.startswith("iwdb.")), values, max_size=5))
def test_any_value_round_trips(store, attr):
    with store.transaction() as tx:
        tx.upsert_node("v", attr=attr, meta=attr)
    node = store.node("v")
    assert same(node["attr"], attr)
    assert same(node["meta"], attr)
