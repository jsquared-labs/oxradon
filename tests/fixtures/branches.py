"""Every construct radon counts as a decision point."""


def conditionals(a, b, c):
    if a and b or c:
        return 1
    elif b:
        return 2
    elif c and not a:
        return 3
    else:
        if a:
            return 4
    return 5 if a else 6


def loops(items):
    total = 0
    for item in items:
        total += item
    else:
        total -= 1
    while total > 10:
        total //= 2
    else:
        total += 1
    async_like = [x for x in items if x if x > 1]
    nested = {k: v for k in items for v in items if k != v}
    gen = sum(y for y in items)
    return total, async_like, nested, gen


def exceptions(f):
    try:
        f()
    except ValueError:
        pass
    except (TypeError, KeyError):
        pass
    else:
        pass
    finally:
        pass
    try:
        f()
    except* OSError:
        pass


def matching(command):
    match command:
        case ["go", direction]:
            return direction
        case {"x": x} if x > 0:
            return x
        case Point(x=0) | Point(y=0):
            return 0
        case other:
            return other


def matching_without_default(command):
    match command:
        case 1:
            return "one"
        case 2:
            return "two"


def lambdas_and_walrus(data):
    key = lambda item: item.a or item.b
    if (n := len(data)) > 3 and key:
        return n
    return sorted(data, key=lambda d: d if d else 0)


async def asynchronous(stream):
    async for chunk in stream:
        if chunk:
            yield chunk
    async with stream as s:
        await s.close()
