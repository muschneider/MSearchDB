"""Bulk operation helpers for MSearchDB.

Provides utilities to construct NDJSON payloads for the ``/_bulk`` endpoint.
"""

from __future__ import annotations

import json
from typing import Any


def build_ndjson(operations: list[dict[str, Any]]) -> str:
    """Convert a list of bulk operation dicts to NDJSON text.

    Each operation dict must have an ``"action"`` key (``"index"`` or
    ``"delete"``) and may include ``"_id"`` and ``"doc"`` (for index).

    Args:
        operations: List of operation descriptors.

    Returns:
        NDJSON-formatted string.

    Example::

        ops = [
            {"action": "index", "_id": "1", "doc": {"name": "Laptop"}},
            {"action": "delete", "_id": "2"},
        ]
        ndjson = build_ndjson(ops)
    """
    lines: list[str] = []
    for op in operations:
        action = op.get("action", "index")
        doc_id = op.get("_id")

        if action == "index":
            meta: dict[str, Any] = {"index": {}}
            if doc_id is not None:
                meta["index"]["_id"] = str(doc_id)
            lines.append(json.dumps(meta, separators=(",", ":")))
            doc = op.get("doc", {})
            lines.append(json.dumps(doc, separators=(",", ":")))
        elif action == "delete":
            meta = {"delete": {}}
            if doc_id is not None:
                meta["delete"]["_id"] = str(doc_id)
            lines.append(json.dumps(meta, separators=(",", ":")))
        else:
            raise ValueError(f"Unknown bulk action: {action!r}")

    # Trailing newline required by NDJSON spec
    return "\n".join(lines) + "\n" if lines else ""


def documents_to_ndjson(documents: list[dict[str, Any]]) -> str:
    """Convert a list of document dicts to NDJSON for bulk indexing.

    Each document may contain an ``"id"`` key which is extracted and used as
    the ``_id`` in the action metadata.  The remaining fields become the
    document body.

    Args:
        documents: List of document dicts.

    Returns:
        NDJSON-formatted string.
    """
    lines: list[str] = []
    for doc in documents:
        doc = dict(doc)  # shallow copy to avoid mutation
        doc_id = doc.pop("id", None)
        meta: dict[str, Any] = {"index": {}}
        if doc_id is not None:
            meta["index"]["_id"] = str(doc_id)
        lines.append(json.dumps(meta, separators=(",", ":")))
        lines.append(json.dumps(doc, separators=(",", ":")))
    return "\n".join(lines) + "\n" if lines else ""
