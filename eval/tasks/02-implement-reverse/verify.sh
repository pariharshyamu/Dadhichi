#!/usr/bin/env bash
python3 -c "import strings; assert strings.reverse('abc') == 'cba'; assert strings.reverse('') == ''; assert strings.reverse('a') == 'a'; assert strings.reverse('racecar') == 'racecar'"
