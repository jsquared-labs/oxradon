async def fetch(a):
    async with s as t:
        return await a + t

class K:
    async def m(self):
        return [x async for x in self if x > 0]
