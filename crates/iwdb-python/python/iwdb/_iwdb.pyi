"""Type stubs for the native module (see documentation/python-api.md)."""

import datetime
import os
from types import TracebackType
from typing import Any, Dict, List, Optional, Sequence, Type, Union

from .filter import Filter

__version__: str

Path = Union[str, "os.PathLike[str]"]
AttrPath = Union[str, Sequence[str]]
# None, bool, int, float, str, bytes, list, dict, date, datetime
Value = Any
CommitResult = Dict[str, Any]
# A query's answer: its results plus "seq", "cursor", "truncated", "work"
Answer = Dict[str, Any]
# A node id or a list of them
Ids = Union[str, Sequence[str]]

class Error(Exception): ...
class ConflictError(Error): ...
class ConstraintError(Error): ...
class NotFoundError(Error): ...
class InvalidError(Error): ...
class ReadOnlyError(Error): ...
class LockedError(Error): ...
class IoError(Error): ...
class CorruptError(Error): ...
class TimeoutError(Error): ...
class ClosedError(Error): ...
class InternalError(Error): ...
class BudgetExceededError(Error): ...
class CursorExpiredError(Error): ...
class NotRetainedError(Error): ...
class UnavailableError(Error): ...
class UnauthenticatedError(Error): ...
class PermissionDeniedError(Error): ...

class _Graph:
    """The methods of one namespace: `Store` (on "default") and `Namespace`."""

    def transaction(self, *, idempotency_key: Optional[str] = None) -> "Transaction": ...
    def node(
        self, id: str, *, min_seq: Optional[int] = None, timeout: Optional[float] = None
    ) -> Optional[Dict[str, Any]]: ...
    def edge(
        self, id: int, *, min_seq: Optional[int] = None, timeout: Optional[float] = None
    ) -> Optional[Dict[str, Any]]: ...
    def wait_for_seq(self, seq: int, *, timeout: Optional[float] = None) -> int: ...
    def seq(self) -> int: ...
    def synced_seq(self) -> Optional[int]: ...
    def read_only(self) -> Optional[str]: ...
    def catalog(self, *, min_seq: Optional[int] = None, timeout: Optional[float] = None) -> Dict[str, Any]: ...
    def indexes(self) -> List[Dict[str, Any]]: ...
    def changes(
        self,
        from_seq: int = 0,
        *,
        wait: bool = False,
        max_results: Optional[int] = None,
        history: Optional[str] = None,
        timeout: Optional[float] = None,
    ) -> Dict[str, Any]: ...
    def create_index(self, path: AttrPath, *, idempotency_key: Optional[str] = None) -> CommitResult: ...
    def drop_index(self, path: AttrPath, *, idempotency_key: Optional[str] = None) -> CommitResult: ...
    def add_constraint(
        self, kind: str, label: str, path: AttrPath, *, idempotency_key: Optional[str] = None
    ) -> CommitResult: ...
    def drop_constraint(
        self, kind: str, label: str, path: AttrPath, *, idempotency_key: Optional[str] = None
    ) -> CommitResult: ...
    # Embedded only (InvalidError on a store from connect)
    def sync(self) -> None: ...
    def checkpoint(self) -> Dict[str, Any]: ...
    def import_file(self, path: Path, *, format: Optional[str] = None) -> Dict[str, Any]: ...
    def export(self, path: Path, *, format: Optional[str] = None) -> Dict[str, Any]: ...
    # Queries (step 14); every answer also has "seq", "cursor", "truncated", "work"
    def find(
        self,
        filter: Filter,
        *,
        max_results: Optional[int] = None,
        max_visited: Optional[int] = None,
        max_edges: Optional[int] = None,
        partial: bool = False,
        cursor: Optional[str] = None,
        min_seq: Optional[int] = None,
        timeout: Optional[float] = None,
    ) -> Answer: ...
    def explain(
        self, filter: Filter, *, analyze: bool = False, min_seq: Optional[int] = None, timeout: Optional[float] = None
    ) -> Answer: ...
    def neighbourhood(
        self,
        seeds: Ids,
        *,
        depth: int = 1,
        direction: str = "out",
        edge_types: Optional[List[str]] = None,
        edge_filter: Optional[Filter] = None,
        node_filter: Optional[Filter] = None,
        max_results: Optional[int] = None,
        max_visited: Optional[int] = None,
        max_edges: Optional[int] = None,
        partial: bool = False,
        cursor: Optional[str] = None,
        min_seq: Optional[int] = None,
        timeout: Optional[float] = None,
    ) -> Answer: ...
    def traverse(
        self,
        start: str,
        *,
        order: str = "bfs",
        depth: Optional[int] = None,
        direction: str = "out",
        edge_types: Optional[List[str]] = None,
        edge_filter: Optional[Filter] = None,
        max_results: Optional[int] = None,
        max_visited: Optional[int] = None,
        max_edges: Optional[int] = None,
        partial: bool = False,
        cursor: Optional[str] = None,
        min_seq: Optional[int] = None,
        timeout: Optional[float] = None,
    ) -> Answer: ...
    def shortest_path(
        self,
        from_: str,
        to: str,
        *,
        method: str = "bfs",
        weight: Optional[str] = None,
        default_weight: float = 1.0,
        coords: Optional[Union[str, Sequence[AttrPath]]] = None,
        metric: str = "euclidean",
        direction: str = "out",
        max_depth: Optional[int] = None,
        max_cost: Optional[float] = None,
        max_results: Optional[int] = None,
        max_visited: Optional[int] = None,
        max_edges: Optional[int] = None,
        partial: bool = False,
        cursor: Optional[str] = None,
        min_seq: Optional[int] = None,
        timeout: Optional[float] = None,
    ) -> Answer: ...
    def random_walks(
        self,
        start: str,
        *,
        max_length: int,
        walks: int = 1,
        min_length: int = 1,
        allow_revisit: bool = False,
        seed: Optional[int] = None,
        max_results: Optional[int] = None,
        max_visited: Optional[int] = None,
        max_edges: Optional[int] = None,
        partial: bool = False,
        cursor: Optional[str] = None,
        min_seq: Optional[int] = None,
        timeout: Optional[float] = None,
    ) -> Answer: ...
    def subgraph(
        self,
        seeds: Ids,
        *,
        depth: int = 1,
        direction: str = "out",
        edge_types: Optional[List[str]] = None,
        edge_filter: Optional[Filter] = None,
        max_results: Optional[int] = None,
        max_visited: Optional[int] = None,
        max_edges: Optional[int] = None,
        partial: bool = False,
        cursor: Optional[str] = None,
        min_seq: Optional[int] = None,
        timeout: Optional[float] = None,
    ) -> Answer: ...
    def match(
        self,
        pattern: str,
        *,
        where: Optional[Dict[str, Filter]] = None,
        max_results: Optional[int] = None,
        max_visited: Optional[int] = None,
        max_edges: Optional[int] = None,
        partial: bool = False,
        cursor: Optional[str] = None,
        min_seq: Optional[int] = None,
        timeout: Optional[float] = None,
    ) -> Answer: ...
    def analyze(
        self,
        job: str,
        *,
        params: Optional[Dict[str, Any]] = None,
        direction: str = "out",
        weight: Optional[str] = None,
        default_weight: float = 1.0,
        max_results: Optional[int] = None,
        max_visited: Optional[int] = None,
        max_edges: Optional[int] = None,
        partial: bool = False,
        cursor: Optional[str] = None,
        min_seq: Optional[int] = None,
        timeout: Optional[float] = None,
    ) -> Answer: ...

class Store(_Graph):
    @staticmethod
    def connect(
        endpoint: str,
        token: Optional[str] = None,
        user: Optional[str] = None,
        password: Optional[str] = None,
        ca: Optional[Path] = None,
        cert: Optional[Path] = None,
        key: Optional[Path] = None,
    ) -> "Store": ...
    @staticmethod
    def open(
        path: Path,
        *,
        create_if_missing: bool = True,
        fsync: str = "always",
        group_max_delay: float = 0.01,
        group_max_batch: int = 64,
        segment_size: int = 64 << 20,
        checkpoint_wal_size: Optional[int] = 256 << 20,
        checkpoint_interval: Optional[float] = 300.0,
        checkpoint_on_close: bool = True,
        checkpoint_keep: int = 2,
        checkpoint_background: bool = True,
        archive: Optional[Path] = None,
        retain_records: int = 0,
        retain_age: Optional[float] = None,
    ) -> "Store": ...
    def close(self) -> None: ...
    @property
    def closed(self) -> bool: ...
    def __enter__(self) -> "Store": ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]] = None,
        exc: Optional[BaseException] = None,
        traceback: Optional[TracebackType] = None,
    ) -> bool: ...
    def create_namespace(self, name: str, *, idempotency_key: Optional[str] = None) -> Dict[str, Any]: ...
    def drop_namespace(self, name: str, *, idempotency_key: Optional[str] = None) -> Dict[str, Any]: ...
    def namespaces(self) -> List[Dict[str, Any]]: ...
    def namespace(self, name: str) -> "Namespace": ...
    # Embedded only (InvalidError on a store from connect)
    def history(self) -> str: ...
    def status(self) -> Dict[str, Any]: ...
    def checkpoint_all(self) -> Dict[str, Dict[str, Any]]: ...
    def backup(self, dest: Path) -> Dict[str, Any]: ...
    def import_namespace(self, name: str, path: Path, *, format: Optional[str] = None) -> Dict[str, Any]: ...

class Namespace(_Graph):
    @property
    def name(self) -> str: ...
    @property
    def id(self) -> int: ...
    def status(self) -> Dict[str, Any]: ...

class Transaction:
    def upsert_node(
        self,
        id: str,
        *,
        labels: Sequence[str] = (),
        attr: Optional[Dict[str, Value]] = None,
        meta: Optional[Dict[str, Value]] = None,
        expected_version: Optional[int] = None,
    ) -> None: ...
    def delete_node(self, id: str, *, expected_version: Optional[int] = None) -> None: ...
    def add_edge(
        self,
        from_: str,
        to: str,
        *,
        type: Optional[str] = None,
        attr: Optional[Dict[str, Value]] = None,
        meta: Optional[Dict[str, Value]] = None,
    ) -> int: ...
    def upsert_edge(
        self,
        *,
        id: Optional[int] = None,
        from_: Optional[str] = None,
        to: Optional[str] = None,
        type: Optional[str] = None,
        attr: Optional[Dict[str, Value]] = None,
        meta: Optional[Dict[str, Value]] = None,
        expected_version: Optional[int] = None,
    ) -> int: ...
    def delete_edge(self, id: int, *, expected_version: Optional[int] = None) -> None: ...
    def set_attr(
        self, target: Union[str, int], key: str, value: Value, *, expected_version: Optional[int] = None
    ) -> None: ...
    def remove_attr(self, target: Union[str, int], key: str, *, expected_version: Optional[int] = None) -> None: ...
    def append_attr(
        self, target: Union[str, int], key: str, value: Value, *, expected_version: Optional[int] = None
    ) -> None: ...
    def add_label(self, id: str, label: str, *, expected_version: Optional[int] = None) -> None: ...
    def remove_label(self, id: str, label: str, *, expected_version: Optional[int] = None) -> None: ...
    def set_edge_type(self, id: int, type: Optional[str], *, expected_version: Optional[int] = None) -> None: ...
    def commit(self) -> CommitResult: ...
    @property
    def result(self) -> Optional[CommitResult]: ...
    def __len__(self) -> int: ...
    def __enter__(self) -> "Transaction": ...
    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]] = None,
        exc: Optional[BaseException] = None,
        traceback: Optional[TracebackType] = None,
    ) -> bool: ...

def verify(path: Path) -> Dict[str, Any]: ...
def restore(
    dest: Path,
    *,
    backup: Optional[Path] = None,
    archive: Optional[Path] = None,
    seq: Optional[int] = None,
    time: Optional[datetime.datetime] = None,
    namespaces: Optional[List[str]] = None,
) -> Dict[str, Any]: ...
