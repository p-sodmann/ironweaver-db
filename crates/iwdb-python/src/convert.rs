//! Python values to database values and back, exactly
//! (`documentation/python-api.md`, "Values").

use ironweaver_core::temporal::Parts;
use ironweaver_core::{Date, DateTime};
use iwdb::{Attrs, Value};
use iwdb_engine::mutation::MAX_VALUE_DEPTH;
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyBool, PyByteArray, PyBytes, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple, PyType};

static DATETIME: PyOnceLock<Py<PyType>> = PyOnceLock::new();
static DATE: PyOnceLock<Py<PyType>> = PyOnceLock::new();
static TIMEDELTA: PyOnceLock<Py<PyType>> = PyOnceLock::new();
static TIMEZONE: PyOnceLock<Py<PyType>> = PyOnceLock::new();

/// The `datetime` module's classes. The bindings use the abi3 (limited)
/// API, which has no datetime C API, so dates go through Python calls.
fn datetime_type(py: Python<'_>) -> PyResult<&Bound<'_, PyType>> {
    DATETIME.import(py, "datetime", "datetime")
}

fn date_type(py: Python<'_>) -> PyResult<&Bound<'_, PyType>> {
    DATE.import(py, "datetime", "date")
}

fn timedelta_type(py: Python<'_>) -> PyResult<&Bound<'_, PyType>> {
    TIMEDELTA.import(py, "datetime", "timedelta")
}

fn timezone_type(py: Python<'_>) -> PyResult<&Bound<'_, PyType>> {
    TIMEZONE.import(py, "datetime", "timezone")
}

fn too_deep() -> PyErr {
    PyValueError::new_err(format!("the value is nested more than {} levels deep (or contains itself)", MAX_VALUE_DEPTH))
}

/// A Python value as a database value. `level` is the value's depth (1 for
/// an attribute's own value). Depth is counted like the commit pipeline
/// does: an empty list or dict counts as holding a scalar.
pub fn to_value(v: &Bound<'_, PyAny>, level: usize) -> PyResult<Value> {
    if level > MAX_VALUE_DEPTH {
        return Err(too_deep());
    }
    let py = v.py();
    Ok(if v.is_none() {
        Value::None
    } else if let Ok(b) = v.cast::<PyBool>() {
        Value::Bool(b.is_true())
    } else if v.is_instance_of::<PyInt>() {
        // OverflowError outside the i64 range
        Value::Int(v.extract::<i64>()?)
    } else if let Ok(f) = v.cast::<PyFloat>() {
        Value::Float(f.value())
    } else if v.is_instance_of::<PyString>() {
        Value::String(v.extract::<String>()?)
    } else if let Ok(b) = v.cast::<PyBytes>() {
        Value::Bytes(b.as_bytes().to_vec())
    } else if let Ok(b) = v.cast::<PyByteArray>() {
        Value::Bytes(b.to_vec())
    } else if let Ok(list) = v.cast::<PyList>() {
        if list.is_empty() && level + 1 > MAX_VALUE_DEPTH {
            return Err(too_deep());
        }
        Value::List(list.iter().map(|item| to_value(&item, level + 1)).collect::<PyResult<_>>()?)
    } else if let Ok(dict) = v.cast::<PyDict>() {
        if dict.is_empty() && level + 1 > MAX_VALUE_DEPTH {
            return Err(too_deep());
        }
        Value::Dict(dict_entries(dict, level + 1)?)
    } else if v.is_instance(datetime_type(py)?)? {
        Value::DateTime(to_datetime(v)?)
    } else if v.is_instance(date_type(py)?)? {
        let (y, m, d): (i32, u32, u32) =
            (v.getattr("year")?.extract()?, v.getattr("month")?.extract()?, v.getattr("day")?.extract()?);
        Value::Date(iwdb_date(y, m, d)?)
    } else if v.is_instance_of::<PyTuple>() {
        return Err(PyTypeError::new_err("a tuple can't be stored (it would read back as a list): use a list"));
    } else {
        let name = v.get_type().name().map(|n| n.to_string()).unwrap_or_default();
        return Err(PyTypeError::new_err(format!(
            "a value of type '{}' can't be stored (None, bool, int, float, str, bytes, list, dict, date, datetime)",
            name
        )));
    })
}

fn iwdb_date(y: i32, m: u32, d: u32) -> PyResult<Date> {
    Date::from_ymd(y, m, d).map_err(|e| PyValueError::new_err(e.to_string()))
}

/// A `datetime.datetime`: an aware one keeps its UTC offset (whole
/// seconds), a naive one stays naive.
fn to_datetime(v: &Bound<'_, PyAny>) -> PyResult<DateTime> {
    let get = |name: &str| -> PyResult<u32> { v.getattr(name)?.extract() };
    let parts = Parts {
        year: v.getattr("year")?.extract()?,
        month: get("month")?,
        day: get("day")?,
        hour: get("hour")?,
        minute: get("minute")?,
        second: get("second")?,
        microsecond: get("microsecond")?,
    };
    let offset = v.call_method0("utcoffset")?;
    let offset = if offset.is_none() {
        None
    } else {
        let micros: i64 = offset.getattr("microseconds")?.extract()?;
        if micros != 0 {
            return Err(PyValueError::new_err("UTC offsets with fractions of a second can't be stored"));
        }
        let days: i64 = offset.getattr("days")?.extract()?;
        let seconds: i64 = offset.getattr("seconds")?.extract()?;
        Some(i32::try_from(days * 86_400 + seconds).map_err(|_| PyValueError::new_err("UTC offset out of range"))?)
    };
    DateTime::from_parts(parts, offset).map_err(|e| PyValueError::new_err(e.to_string()))
}

/// A dict's entries; every key must be a `str`.
fn dict_entries(dict: &Bound<'_, PyDict>, level: usize) -> PyResult<Attrs> {
    let mut out = Attrs::with_capacity(dict.len());
    for (key, value) in dict.iter() {
        if !key.is_instance_of::<PyString>() {
            let name = key.get_type().name().map(|n| n.to_string()).unwrap_or_default();
            return Err(PyTypeError::new_err(format!("dict keys must be str, not '{}'", name)));
        }
        out.insert(key.extract::<String>()?, to_value(&value, level)?);
    }
    Ok(out)
}

/// An attribute or meta dict (`None`: empty).
pub fn to_attrs(v: Option<&Bound<'_, PyAny>>) -> PyResult<Attrs> {
    match v {
        None => Ok(Attrs::new()),
        Some(v) if v.is_none() => Ok(Attrs::new()),
        Some(v) => {
            let dict = v.cast::<PyDict>().map_err(|_| PyTypeError::new_err("attributes and meta must be a dict"))?;
            dict_entries(dict, 1)
        }
    }
}

/// A database value as a Python value.
pub fn from_value<'py>(py: Python<'py>, v: &Value) -> PyResult<Bound<'py, PyAny>> {
    Ok(match v {
        Value::None => py.None().into_bound(py),
        Value::Bool(b) => PyBool::new(py, *b).to_owned().into_any(),
        Value::Int(i) => i.into_pyobject(py)?.into_any(),
        Value::Float(f) => PyFloat::new(py, *f).into_any(),
        Value::Half(h) => PyFloat::new(py, h.to_f64()).into_any(),
        Value::String(s) => PyString::new(py, s).into_any(),
        Value::Bytes(b) => PyBytes::new(py, b).into_any(),
        Value::List(items) => {
            let items = items.iter().map(|item| from_value(py, item)).collect::<PyResult<Vec<_>>>()?;
            PyList::new(py, items)?.into_any()
        }
        Value::Dict(entries) => from_attrs(py, entries)?.into_any(),
        Value::Date(d) => {
            let (y, m, day) = d.ymd();
            date_type(py)?.call1((y, m, day))?
        }
        Value::DateTime(t) => {
            let p = t.parts();
            let tz = match t.offset {
                Some(offset) => timezone_type(py)?.call1((timedelta_type(py)?.call1((0, offset))?,))?,
                None => py.None().into_bound(py),
            };
            datetime_type(py)?.call1((p.year, p.month, p.day, p.hour, p.minute, p.second, p.microsecond, tz))?
        }
    })
}

/// An attribute map as a dict, keys sorted.
pub fn from_attrs<'py>(py: Python<'py>, attrs: &Attrs) -> PyResult<Bound<'py, PyDict>> {
    let mut keys: Vec<&String> = attrs.keys().collect();
    keys.sort_unstable();
    let dict = PyDict::new(py);
    for key in keys {
        if let Some(value) = attrs.get(key) {
            dict.set_item(key, from_value(py, value)?)?;
        }
    }
    Ok(dict)
}
