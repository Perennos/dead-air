"""Compatibility entry point; implementation lives in the deadair package."""
import sys
from deadair import engine as _engine

if __name__ == "__main__":
    raise SystemExit(_engine.main())
else:
    # Preserve the original import API, including test seams and embedded callers.
    sys.modules[__name__] = _engine
