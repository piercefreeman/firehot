import random


ADJECTIVES: list[str] = [
    "bright",
    "colorful",
    "ancient",
    "gentle",
    "brave",
    "clever",
    "dazzling",
    "eager",
    "fierce",
    "graceful",
    "happy",
    "infinite",
    "jolly",
    "kind",
    "luminous",
    "mystical",
    "noble",
    "peaceful",
    "quick",
    "radiant",
    "silent",
    "thoughtful",
    "unique",
    "vibrant",
    "wise",
    "zesty",
    "calm",
    "elegant",
    "golden",
    "humble"
]

NOUNS: list[str] = [
    "windmill",
    "flower",
    "mountain",
    "river",
    "forest",
    "ocean",
    "castle",
    "village",
    "garden",
    "horizon",
    "island",
    "journey",
    "kingdom",
    "lake",
    "meadow",
    "notebook",
    "orchard",
    "pathway",
    "quilt",
    "rainbow",
    "sunset",
    "treasure",
    "valley",
    "waterfall",
    "cloud",
    "bridge",
    "desert",
    "harbor",
    "lighthouse",
    "canyon"
]

class NameManager:
    def __init__(self, separator: str = "-"):
        self.separator = separator
        self.reserved_names = set()

    def reserve_random_name(self) -> str:
        """
        Reserve a random name from the pool of available names. Ensures uniqueness
        across multiple exec runs.

        """
        if len(self.reserved_names) >= len(ADJECTIVES) * len(NOUNS):
            raise ValueError("No more names available")

        while True:
            random_adjective = random.choice(ADJECTIVES)
            random_noun = random.choice(NOUNS)
            
            proposed_name = f"{random_adjective}{self.separator}{random_noun}"

            if proposed_name in self.reserved_names:
                continue

            # Reserve the name
            self.reserved_names.add(proposed_name)
            return proposed_name

    def release_name(self, name: str) -> None:
        """
        Release a name back to the pool of available names.

        """
        self.reserved_names.remove(name)

# Singleton instance
NAME_REGISTRY = NameManager()
