"""Indentation with tabs. CPython measures it with tab stops of 8 and of 1."""


def tabs(a, b):
	if a:
		if b:
			return 1
		return 2
	return 3


def one_level_spelled_two_ways(a, b):
    		if a:
 	   			return 1
 	   	if b:
    				return 2
    		return 3


class C:
	def m(self, a):
		for i in a:
		        if i:
			        return i
		return [
	0,
  1,
			]

	def n(self, a):
		return a or \
 0
