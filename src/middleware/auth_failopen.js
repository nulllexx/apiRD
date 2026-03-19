const jwt = require('jsonwebtoken');

module.exports = (req, res, next) => {
    const publicEndpoints = ['/login', '/register'];

    // Skip authentication for public routes
    if (publicEndpoints.includes(req.path)) {
        return next();
    }
    const userToken = req.cookies.userToken;

    // Verify the chosen token
    jwt.verify(userToken, process.env.JWT_SECRET, (err, decoded) => {
        if (err) {
            next();
        }

        req.user = decoded;
        
        console.log('Decoded Token:', decoded);
        next();
    });
};