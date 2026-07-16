def fizzbuzz(n):
    # BUG: the divisible-by-3 check runs first, so multiples of 15 return
    # "Fizz" instead of "FizzBuzz". Fix the ordering / logic.
    if n % 3 == 0:
        return "Fizz"
    if n % 5 == 0:
        return "Buzz"
    if n % 15 == 0:
        return "FizzBuzz"
    return str(n)
