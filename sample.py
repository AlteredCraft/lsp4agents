def greet(name: str) -> str:
    # Decoy: the word greet appears in this comment but must NOT be renamed.
    return f"Hello, {name}! The string also says greet on purpose."


message = greet(123)
print(message)
