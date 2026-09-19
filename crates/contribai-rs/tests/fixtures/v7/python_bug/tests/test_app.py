import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import app


def test_add():
    assert app.add(2, 3) == 5


def test_describe():
    assert app.describe() == "adder"
