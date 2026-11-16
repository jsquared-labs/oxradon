import sys

if sys.argv and len(sys.argv) > 1 or not sys.argv:
    def inside_if(x):
        return [y for y in x if y if y > 1]
elif sys.platform == "x":
    pass
else:
    x = 1 if sys.argv else 2

for i in range(3):
    pass
else:
    pass

while False:
    break

try:
    import nothing
except ImportError:
    pass
except Exception:
    pass
else:
    pass
finally:
    pass

try:
    pass
except* ValueError:
    pass

assert sys.argv, "assert counts at module level"

match sys.argv:
    case [a]:
        pass
    case [a, b]:
        pass
    case _:
        pass

squares = {k: v for k in range(3) for v in range(k) if v}
gen = (z for z in range(3))
f = lambda q: q and q or not q


@staticmethod
def decorated(a, b=[i for i in range(2) if i]):
    if a:
        def closure():
            if b:
                return 1
            return 2
        return closure
    return None


class Outer:
    if True:
        attr = 1

    def method(self):
        for _ in self:
            while self:
                break
        return [w for w in self if w]

    class Inner:
        def inner_method(self):
            return 1 if self else 0


with open(__file__) as fh:
    def in_with():
        return fh and 1
