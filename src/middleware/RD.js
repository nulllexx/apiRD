const jwt = require('jsonwebtoken');
const { queryOne } = require('../db');

module.exports = async function RDadmin(req, res, next) {
    const token = req.cookies.userToken;
    if (!token) {
        console.log("No cookie");
        return res.redirect('https://bakosmp.go.ro/raindrippy/login.html');
    }

    try {
        const payload = jwt.verify(token, process.env.JWT_SECRET);
        
        // Look up the user in the DB using queryOne from your db.js
        const user = await queryOne(
            'SELECT id, username, is_admin FROM users WHERE id = ?',
            [payload.id]
        );

        if (!user) {
            console.log("User not found");
            return res.redirect('https://bakosmp.go.ro/raindrippy/login.html');
        }

        if (!user.is_admin) {
            console.log("User not an admin");
            return res.redirect('https://bakosmp.go.ro/raindrippy/login.html');
        }

        req.user = payload;
        next();
    } catch (err) {
        console.error('privAuth error:', err);
        return res.redirect('https://bakosmp.go.ro/raindrippy/login.html');
    }
};