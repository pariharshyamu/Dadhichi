#!/usr/bin/env bash
# Passes (exit 0) iff add() now returns the correct sum.
python3 -c "import calc; assert calc.add(2, 3) == 5; assert calc.add(-1, 1) == 0; assert calc.add(0, 0) == 0"
