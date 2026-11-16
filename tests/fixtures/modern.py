type Pair[T] = tuple[T, T]


def first[T](items: list[T]) -> T | None:
    return items[0] if items else None


class Box[T]:
    def __init__(self, item: T) -> None:
        self.item = item

    def get(self) -> T:
        return self.item if self.item is not None else self.default()


def templates(name):
    greeting = t"hello {name}" t" and {name!r:>10}"
    return greeting


def unicode_names(café, naïve=None):
    résumé = café or naïve
    return f"{résumé} ✓"
