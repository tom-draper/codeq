module Analysis where
import qualified Data.List as List
-- Compute a score.
score :: Int -> Int
score 0 = 0
score n = n + 1
data Point = Point Int Int
newtype Count = Count Int
type Name = String
class Render a where
  render :: a -> String
label = "point"
