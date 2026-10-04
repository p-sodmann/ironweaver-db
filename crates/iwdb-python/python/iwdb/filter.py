"""Filters for the query methods: Python expressions that become the core's
`Expr` (documentation/python-api.md, "Filters").

    iwdb.label("Person") & (iwdb.attr("age") >= 18)
    iwdb.attr(["address", "city"]).isin(["Berlin", "Paris"])
    ~iwdb.attr("email").exists()

A filter holds plain data (`_expr`, a tuple); the bindings convert it, and
its values convert like attribute values. A filter nests at most 100 levels
(And, Or and Not around a leaf, which counts as one: the core's limit);
deeper ones raise `ValueError` when used.
"""

from typing import Any, Iterable, List, Sequence, Union

AttrPath = Union[str, Sequence[str]]

__all__ = ["Attr", "Filter", "attr", "const", "edge_type", "label"]


class Filter:
    """A predicate over a node or an edge. Combine with `&`, `|` and `~`."""

    __slots__ = ("_expr",)

    def __init__(self, expr: tuple) -> None:
        self._expr = expr

    def __and__(self, other: "Filter") -> "Filter":
        if not isinstance(other, Filter):
            return NotImplemented
        return Filter(("and", _operands("and", self, other)))

    def __or__(self, other: "Filter") -> "Filter":
        if not isinstance(other, Filter):
            return NotImplemented
        return Filter(("or", _operands("or", self, other)))

    def __invert__(self) -> "Filter":
        return Filter(("not", self))

    def __bool__(self) -> bool:
        # `a and b`, `not a` and chained comparisons (`1 < attr("x") < 3`)
        # would silently drop a part of the filter
        raise TypeError("a filter has no truth value: combine filters with &, | and ~")

    def __repr__(self) -> str:
        kind = self._expr[0]
        if kind in ("and", "or"):
            sep = " & " if kind == "and" else " | "
            return "(" + sep.join(repr(f) for f in self._expr[1]) + ")"
        if kind == "not":
            return "~" + repr(self._expr[1])
        if kind == "compare":
            symbol = {"eq": "==", "ne": "!=", "lt": "<", "le": "<=", "gt": ">", "ge": ">="}[self._expr[2]]
            return "attr({!r}) {} {!r}".format(self._expr[1], symbol, self._expr[3])
        if kind == "in":
            return "attr({!r}).isin({!r})".format(self._expr[1], self._expr[2])
        if kind == "exists":
            return "attr({!r}).exists()".format(self._expr[1])
        if kind == "type":
            return "edge_type({!r})".format(self._expr[1])
        return "{}({!r})".format(kind, self._expr[1])


def _operands(kind: str, left: Filter, right: Filter) -> List[Filter]:
    """The operands of `left kind right`, flattened: `a & b & c` is one And
    of three, not two nested ones."""
    operands: List[Filter] = []
    for f in (left, right):
        if f._expr[0] == kind:
            operands.extend(f._expr[1])
        else:
            operands.append(f)
    return operands


class Attr:
    """The attribute at a path (a key, or a key and then keys into nested
    dicts). Compare it with a value, or use `isin` and `exists`."""

    __slots__ = ("_path",)
    __hash__ = None  # type: ignore[assignment]

    def __init__(self, path: AttrPath) -> None:
        self._path = [path] if isinstance(path, str) else list(path)

    def _compare(self, op: str, value: Any) -> Filter:
        return Filter(("compare", self._path, op, value))

    def __eq__(self, value: Any) -> Filter:  # type: ignore[override]
        return self._compare("eq", value)

    def __ne__(self, value: Any) -> Filter:  # type: ignore[override]
        return self._compare("ne", value)

    def __lt__(self, value: Any) -> Filter:
        return self._compare("lt", value)

    def __le__(self, value: Any) -> Filter:
        return self._compare("le", value)

    def __gt__(self, value: Any) -> Filter:
        return self._compare("gt", value)

    def __ge__(self, value: Any) -> Filter:
        return self._compare("ge", value)

    def isin(self, values: Iterable[Any]) -> Filter:
        """The attribute equals one of `values`."""
        return Filter(("in", self._path, list(values)))

    def exists(self) -> Filter:
        """The attribute exists and is not None."""
        return Filter(("exists", self._path))

    def __repr__(self) -> str:
        return "attr({!r})".format(self._path)


def attr(path: AttrPath) -> Attr:
    """The attribute at `path`: a `str`, or a list of `str` (a key, then keys
    into nested dicts). Comparisons hold between two numbers, two strings or
    two bools; `==` and `!=` between any values."""
    return Attr(path)


def label(name: str) -> Filter:
    """Nodes with this label (always false for edges)."""
    return Filter(("label", name))


def edge_type(name: str) -> Filter:
    """Edges of this type (always false for nodes)."""
    return Filter(("type", name))


def const(value: bool) -> Filter:
    """Always true, or always false."""
    return Filter(("const", bool(value)))
