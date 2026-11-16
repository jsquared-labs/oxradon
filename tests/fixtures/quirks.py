# Behaviours radon has that a fresh implementation would likely get "wrong".


@decorator_with_logic(lambda x: x or x and x)
# a comment between decorators
@another(
    1 if True else 2,
)
def decorated(a=1 if True else 0, b=lambda: c or d):
    """Decorators and defaults are never visited by radon."""
    return a


def asserts(x):
    assert x and x or x, "boolean operators inside assert are not counted"
    assert x
    return x


def ends_with_nested_def():
    value = 1

    def inner():
        return value

    # endline comes only from the last statement: a def counts for nothing
    def last():
        if value:
            return 2


def ends_with_assert():
    x = 1
    y = 2
    assert x and y


def ends_with_concatenated_fstrings(self):
    return ("start "
            f"{self.a} middle "
            f"tail on its own line"
            ")")


def outer():
    class LocalClass:  # classes inside functions are dropped by radon
        def method(self):
            if self:
                return 1

    def closure(a):
        def deeper(b):
            return b or a
        return deeper

    return closure


class Empty:
    pass


class Outer:
    x = 1 if y else 2

    def first(self):
        if self.x:
            return 1

    if CONDITION:
        def conditional_method(self):
            return 2

    class Inner:
        def inner_method(self):
            for i in range(3):
                pass

        class Deepest:
            def deepest_method(self):
                return 1 and 2

    @property
    def prop(self):
        return self.x


class Child(Outer, metaclass=Meta):
    def only(self):
        while True:
            break
