#!/usr/bin/env bash
python3 -c "
import fizzbuzz as f
assert f.fizzbuzz(15) == 'FizzBuzz', f.fizzbuzz(15)
assert f.fizzbuzz(30) == 'FizzBuzz', f.fizzbuzz(30)
assert f.fizzbuzz(3) == 'Fizz', f.fizzbuzz(3)
assert f.fizzbuzz(5) == 'Buzz', f.fizzbuzz(5)
assert f.fizzbuzz(7) == '7', f.fizzbuzz(7)
"
