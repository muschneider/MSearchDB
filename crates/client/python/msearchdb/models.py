"""Data classes representing MSearchDB responses."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any


# ---------------------------------------------------------------------------
# Search result models
# ---------------------------------------------------------------------------

@dataclass
class Hit:
    """A single search hit."""

    id: str
    score: float | None
    source: dict[str, Any]
    highlight: dict[str, list[str]] | None = None

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> Hit:
        return cls(
            id=data.get("_id", ""),
            score=data.get("_score"),
            source=data.get("_source", {}),
            highlight=data.get("highlight"),
        )


@dataclass
class SearchResult:
    """Result of a search operation."""

    total: int
    total_relation: str
    hits: list[Hit]
    took: int
    timed_out: bool

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> SearchResult:
        hits_obj = data.get("hits", {})
        total_obj = hits_obj.get("total", {})
        raw_hits = hits_obj.get("hits", [])
        return cls(
            total=total_obj.get("value", 0),
            total_relation=total_obj.get("relation", "eq"),
            hits=[Hit.from_dict(h) for h in raw_hits],
            took=data.get("took", 0),
            timed_out=data.get("timed_out", False),
        )

    def __len__(self) -> int:
        return len(self.hits)

    def __iter__(self):
        return iter(self.hits)


# ---------------------------------------------------------------------------
# Bulk result models
# ---------------------------------------------------------------------------

@dataclass
class BulkItemResult:
    """Result of a single item within a bulk operation."""

    action: str
    id: str
    status: int
    error: str | None = None

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> BulkItemResult:
        return cls(
            action=data.get("action", ""),
            id=data.get("_id", ""),
            status=data.get("status", 0),
            error=data.get("error"),
        )

    @property
    def ok(self) -> bool:
        return 200 <= self.status < 300


@dataclass
class BulkResult:
    """Result of a bulk operation."""

    took: int
    errors: bool
    items: list[BulkItemResult] = field(default_factory=list)

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> BulkResult:
        return cls(
            took=data.get("took", 0),
            errors=data.get("errors", False),
            items=[BulkItemResult.from_dict(i) for i in data.get("items", [])],
        )

    @property
    def succeeded(self) -> list[BulkItemResult]:
        return [i for i in self.items if i.ok]

    @property
    def failed(self) -> list[BulkItemResult]:
        return [i for i in self.items if not i.ok]


# ---------------------------------------------------------------------------
# Collection info model
# ---------------------------------------------------------------------------

@dataclass
class CollectionInfo:
    """Metadata about a collection."""

    name: str
    doc_count: int

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> CollectionInfo:
        return cls(
            name=data.get("name", ""),
            doc_count=data.get("doc_count", 0),
        )


# ---------------------------------------------------------------------------
# Index / update / delete result model
# ---------------------------------------------------------------------------

@dataclass
class IndexResult:
    """Result of an index, update, or delete operation."""

    id: str
    result: str
    version: int | None = None

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> IndexResult:
        return cls(
            id=data.get("_id", ""),
            result=data.get("result", ""),
            version=data.get("version"),
        )
