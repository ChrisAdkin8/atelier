import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from pipeline import pipeline


def test_pipeline_parses_scores_and_reports():
    raw = "alice, 92\nbob, 75\n# comment\n\ncarol, 60"
    r = pipeline(raw)
    assert len(r["rows"]) == 3
    assert r["rows"][0] == {"name": "alice", "score": 92, "grade": "A"}
    assert r["rows"][1] == {"name": "bob", "score": 75, "grade": "C"}
    assert r["rows"][2] == {"name": "carol", "score": 60, "grade": "F"}
    assert "alice: 92 (A)" in r["report"]
    assert "carol: 60 (F)" in r["report"]


def test_pipeline_skips_malformed():
    r = pipeline("not,a,row\n,\nbad,xx")
    assert r["rows"] == []
    assert r["report"] == ""


def test_pipeline_empty_input():
    r = pipeline("")
    assert r == {"rows": [], "report": ""}
