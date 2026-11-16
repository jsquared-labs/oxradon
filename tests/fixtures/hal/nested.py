def outer(a):
    def inner(b):
        return b + 1
    return a + inner(a)
